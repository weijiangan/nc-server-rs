//! Schema awareness — the replacement for the disabled `sqlx::migrate!()`.
//!
//! PHP owns the schema entirely ("PHP installs and upgrades, Rust serves" —
//! see SPECS/02-specifications/improvements.md §I.3).  Rust never issues DDL
//! against a live database: no migrations, no `CREATE TABLE`, no `ALTER`.
//! That contract is enforced at the module level — this module only *reads*
//! `information_schema` / `PRAGMA table_info` to compare the running schema
//! against the tables and columns Rust actually reads or writes.
//!
//! The table/column list below is **PHP-grounded**: every entry is derived
//! from the Doctrine migrations in `workspace/server/` (the same migrations
//! that create the live schema), not from the Rust migration files under
//! `core-rs/migrations/` — those are stale in places (e.g. they put
//! `creation_time`/`upload_time` on `oc_filecache`, which PHP keeps only on
//! `oc_filecache_extended`).
//!
//! # Usage
//!
//! This is a diagnostic, deliberately **not** a boot-time gate: it is invoked
//! on demand (the `check-schema` subcommand) so an operator can see whether a
//! PHP-provisioned database matches what this server was built against,
//! without failing startup (phase-9 9.9 deviation: "validate on startup" was
//! deliberately dropped in favour of a manual command).

use std::collections::HashMap;

use crate::pool::DbPool;
use sqlx::Row;

/// One table Rust reads or writes, with the critical columns it touches.
///
/// Table names are **unprefixed**; [`validate_schema`] prepends the configured
/// `dbtableprefix` (default `oc_`), exactly like every query site.
pub struct SchemaRequirement {
    pub table: &'static str,
    pub columns: &'static [&'static str],
}

/// The tables + critical columns `nc-server` depends on.
///
/// The §9.1 gap tables (`files_trash`, `vcategory`, `vcategory_to_object`)
/// are PHP-app-owned (`files_trashbin`, core `tags`) and are listed here too:
/// Rust reads/writes them (trash on DELETE, favorites/tags PROPFIND + PROPPATCH),
/// so a missing one must be reported — and the fix is always PHP-side
/// (`occ app:enable files_trashbin` / a full install), never Rust DDL.
pub const SCHEMA: &[SchemaRequirement] = &[
    // ── core + files (REQ §9.1–§9.8) ──────────────────────────────────────
    SchemaRequirement {
        table: "mimetypes",
        columns: &["id", "mimetype"],
    },
    SchemaRequirement {
        table: "storages",
        columns: &["numeric_id", "id"],
    },
    SchemaRequirement {
        table: "filecache",
        columns: &[
            "fileid",
            "storage",
            "path",
            "path_hash",
            "parent",
            "name",
            "mimetype",
            "mimepart",
            "size",
            "mtime",
            "storage_mtime",
            "etag",
            "permissions",
            "checksum",
        ],
    },
    SchemaRequirement {
        table: "filecache_extended",
        columns: &["fileid", "metadata_etag", "creation_time", "upload_time"],
    },
    SchemaRequirement {
        table: "files_metadata",
        columns: &["file_id", "json"],
    },
    SchemaRequirement {
        table: "users",
        columns: &["uid", "displayname", "password", "uid_lower"],
    },
    SchemaRequirement {
        table: "accounts",
        columns: &["uid", "data"],
    },
    SchemaRequirement {
        table: "group_user",
        columns: &["gid", "uid"],
    },
    SchemaRequirement {
        table: "authtoken",
        columns: &[
            "id",
            "uid",
            "login_name",
            "token",
            "type",
            "scope",
            "expires",
            "last_activity",
        ],
    },
    SchemaRequirement {
        table: "bruteforce_attempts",
        columns: &["action", "occurred", "ip", "subnet", "metadata"],
    },
    SchemaRequirement {
        table: "appconfig",
        columns: &["appid", "configkey", "configvalue", "type", "lazy"],
    },
    SchemaRequirement {
        table: "preferences",
        columns: &["userid", "appid", "configkey", "configvalue"],
    },
    SchemaRequirement {
        table: "properties",
        columns: &[
            "userid",
            "propertypath",
            "propertyname",
            "propertyvalue",
            "valuetype",
        ],
    },
    SchemaRequirement {
        table: "share",
        columns: &[
            "file_source",
            "share_type",
            "share_with",
            "uid_owner",
            "uid_initiator",
            "note",
            "stime",
            "permissions",
        ],
    },
    SchemaRequirement {
        table: "previews",
        columns: &[
            "id",
            "file_id",
            "storage_id",
            "width",
            "height",
            "mimetype_id",
            "source_mimetype_id",
            "max",
            "cropped",
            "encrypted",
            "etag",
            "mtime",
            "size",
            "version_id",
        ],
    },
    SchemaRequirement {
        table: "preview_generation",
        columns: &["uid", "file_id", "queued_at"],
    },
    SchemaRequirement {
        table: "comments",
        columns: &[
            "object_type",
            "object_id",
            "actor_type",
            "actor_id",
            "creation_timestamp",
        ],
    },
    SchemaRequirement {
        table: "comments_read_markers",
        columns: &["user_id", "object_type", "object_id", "marker_datetime"],
    },
    SchemaRequirement {
        table: "systemtag",
        columns: &["id", "name", "visibility", "editable", "color"],
    },
    SchemaRequirement {
        table: "systemtag_object_mapping",
        columns: &["systemtagid", "objectid", "objecttype"],
    },
    // ── §9.1 gap tables (PHP-app-owned — never created by Rust) ────────────
    SchemaRequirement {
        table: "files_trash",
        columns: &[
            "auto_id",
            "id",
            "user",
            "timestamp",
            "location",
            "type",
            "mime",
            "deleted_by",
        ],
    },
    SchemaRequirement {
        table: "vcategory",
        columns: &["id", "uid", "type", "category"],
    },
    SchemaRequirement {
        table: "vcategory_to_object",
        columns: &["objid", "categoryid", "type"],
    },
    SchemaRequirement {
        table: "files_versions",
        columns: &["id", "file_id", "timestamp", "size", "mimetype", "metadata"],
    },
];

/// Outcome of a schema comparison.  `is_ok()` is true only when every
/// required table *and* every critical column exists.
#[derive(Debug, Default)]
pub struct SchemaReport {
    /// Fully-prefixed table names that are missing entirely.
    pub missing_tables: Vec<String>,
    /// `(fully-prefixed table, column)` pairs missing from an existing table.
    pub missing_columns: Vec<(String, String)>,
}

impl SchemaReport {
    pub fn is_ok(&self) -> bool {
        self.missing_tables.is_empty() && self.missing_columns.is_empty()
    }
}

/// Compare the live database against [`SCHEMA`].
///
/// Read-only: only queries catalog metadata (`information_schema` on
/// PostgreSQL, `sqlite_master` + `PRAGMA table_info` on SQLite).  Returns a
/// [`SchemaReport`] describing any missing tables/columns; the caller decides
/// how to surface it (the `check-schema` command prints it and exits non-zero
/// on mismatch — nothing here aborts a running server).
pub async fn validate_schema(pool: &DbPool, prefix: &str) -> anyhow::Result<SchemaReport> {
    // Fully-prefixed table names Rust expects, one per requirement.
    let required: Vec<String> = SCHEMA
        .iter()
        .map(|r| format!("{prefix}{}", r.table))
        .collect();

    // Existing table → its columns, restricted to the required set.
    let existing = existing_columns(pool, &required).await?;

    let mut report = SchemaReport::default();
    for req in SCHEMA {
        let table = format!("{prefix}{}", req.table);
        let Some(cols) = existing.get(&table) else {
            report.missing_tables.push(table);
            continue;
        };
        for col in req.columns {
            if !cols.iter().any(|c| c == col) {
                report
                    .missing_columns
                    .push((table.clone(), (*col).to_string()));
            }
        }
    }
    Ok(report)
}

/// Fetch the columns of the given tables from the catalog.
async fn existing_columns(
    pool: &DbPool,
    required: &[String],
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    match pool {
        DbPool::Pg(p) => {
            // One catalog query for every table we care about.
            let rows = sqlx::query(
                "SELECT table_name, column_name \
                 FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name = ANY($1)",
            )
            .bind(required)
            .fetch_all(p)
            .await?;
            for row in rows {
                let table: String = row.get("table_name");
                let column: String = row.get("column_name");
                map.entry(table).or_default().push(column);
            }
        }
        DbPool::Sqlite(p) => {
            for table in required {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name = $1",
                )
                .bind(table)
                .fetch_one(p)
                .await?;
                if count == 0 {
                    continue;
                }
                // `PRAGMA table_info` cannot take bind parameters; the name is
                // `{prefix}` + a static table name, so double-quote-escape it.
                let quoted = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
                let rows = sqlx::query(&quoted).fetch_all(p).await?;
                map.insert(
                    table.clone(),
                    rows.iter().map(|r| r.get::<String, _>("name")).collect(),
                );
            }
        }
    }
    Ok(map)
}
