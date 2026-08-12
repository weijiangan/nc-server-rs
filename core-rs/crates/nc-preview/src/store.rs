//! `oc_previews` access: the row model, byte-path construction, and the
//! in-memory max/match selection used by the serve path.
//!
//! Parity sources (all verified against live PHP, golden vectors in the tests):
//! - row shape — `lib/private/Preview/Db/Preview.php` + REQ §9.10;
//! - byte path — `LocalPreviewStorage::constructPath`
//!   (`{datadir}/appdata_{instanceid}/preview/{md5(file_id)[0..7] as nested
//!   dirs}/{file_id}/{name}`);
//! - file name — `Preview::getName()` (`[version-]{w}-{h}[-crop][-max].{ext}`)
//!   and `Preview::getExtension()` (output mime → `png`/`gif`/`jpg`/`webp`);
//! - max/match selection — `Generator::getMaxPreview` / `generatePreviews`'s
//!   `array_find` (`Generator.php:168-170, 323-349`).
//!
//! Scope: **local-disk, un-versioned** files (`version_id = -1`), per the Phase 11
//! local-storage assumption.  Object-store (`oc_preview_locations`) and S3-versioned
//! naming are out of scope and fall back to PHP-FPM.

use nc_db::pool::DbPool;
use sqlx::Row;
use std::path::{Path, PathBuf};

/// A row from `oc_previews` (REQ §9.10) — the subset needed for serving (11.2)
/// and persistence (11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRow {
    /// Snowflake primary key (`SnowflakeAwareEntity`).
    pub id: i64,
    /// `oc_filecache.fileid` of the source file.
    pub file_id: i64,
    /// Numeric storage id of the source mount.
    pub storage_id: i64,
    /// Actual produced pixel width.
    pub width: u32,
    /// Actual produced pixel height.
    pub height: u32,
    /// **Output** mimetype id (the preview image's mime), via `oc_mimetypes`.
    pub mimetype_id: i32,
    /// Source file's mimetype id.
    pub source_mimetype_id: i32,
    /// **Generation** timestamp (not the source file's mtime).
    pub mtime: i64,
    /// Byte size of the stored preview.
    pub size: i64,
    /// `true` only for the single max preview a file's other sizes derive from.
    pub max: bool,
    /// Whether this variant was cropped.
    pub cropped: bool,
    /// Always `false` today (previews are stored unencrypted).
    pub encrypted: bool,
    /// The **source file's etag at generation** (not the preview bytes' etag).
    pub etag: String,
    /// `-1` for un-versioned (local disk); else the matching `oc_preview_versions` id.
    pub version_id: i64,
}

impl PreviewRow {
    /// The on-disk file name (`Preview::getName`): `[version-]{w}-{h}[-crop][-max].{ext}`.
    ///
    /// `output_mime` is the row's output mimetype string (resolved from
    /// [`PreviewRow::mimetype_id`] via the mimetype map).  The version prefix is
    /// emitted only when `version_id > -1` (PHP `getVersion() > -1`; verified
    /// `null`/`-1` ⇒ no prefix), which never fires for local-disk files.
    pub fn name(&self, output_mime: &str) -> String {
        preview_name(
            self.version_id,
            self.width,
            self.height,
            self.cropped,
            self.max,
            output_mime,
        )
    }

    /// Absolute path to this preview's bytes (`LocalPreviewStorage::constructPath`).
    pub fn byte_path(&self, datadir: &Path, instanceid: &str, output_mime: &str) -> PathBuf {
        preview_byte_path(datadir, instanceid, self.file_id, &self.name(output_mime))
    }

    /// PHP `array_find` match (`Generator.php:168-170`): a row satisfies a request
    /// when width, height, cropped, **output** mimetype, and version all match.
    /// `max_mimetype_id` is the max preview's output mimetype id (the requested
    /// variant must share the max row's output format).
    pub fn matches(
        &self,
        width: u32,
        height: u32,
        cropped: bool,
        max_mimetype_id: i32,
        version_id: i64,
    ) -> bool {
        self.width == width
            && self.height == height
            && self.cropped == cropped
            && self.mimetype_id == max_mimetype_id
            && self.version_id == version_id
    }
}

/// Find the max preview row for a version (`Generator::getMaxPreview`'s row scan:
/// the first row with `max == true` whose version matches).  Returns `None` when no
/// max row exists yet — the caller then generates one (11.4).
pub fn find_max(rows: &[PreviewRow], version_id: i64) -> Option<&PreviewRow> {
    rows.iter().find(|r| r.max && r.version_id == version_id)
}

/// Find a cached variant matching the bucketed request (`generatePreviews`'s
/// `array_find`).  Returns `None` on a miss — the caller derives it from the max
/// preview (11.4) or proxies to PHP-FPM.
pub fn find_match(
    rows: &[PreviewRow],
    width: u32,
    height: u32,
    cropped: bool,
    max_mimetype_id: i32,
    version_id: i64,
) -> Option<&PreviewRow> {
    rows.iter()
        .find(|r| r.matches(width, height, cropped, max_mimetype_id, version_id))
}

/// Load every preview row for a file (`PreviewMapper::getAvailablePreviews` filters
/// on `file_id` only — `Generator::generatePreviews` fetches all of a file's rows
/// and selects in memory).  Rows come back in DB order; callers use [`find_max`] /
/// [`find_match`] to pick the one to serve.
///
/// `max`/`cropped`/`encrypted` are selected as plain columns and decoded **natively
/// as `bool` on PostgreSQL** (their real column type — best-in-class, no projection
/// on this hot path).  SQLite has no boolean type and sqlx's `Any` driver
/// cannot decode one, so there — and only there, the test/legacy DB — they are read
/// as integers (`!= 0`).  `id`/`width`/`height` are read 64-bit and narrowed
/// (snowflake ids and pixel dimensions fit comfortably for the foreseeable future).
///
/// `etag` is `CHAR(40)` (fixed, blank-padded — see [`ETAG_CHAR_WIDTH`]).  sqlx's
/// `Any` driver has **no mapping for PostgreSQL's `bpchar`**, so a bare `etag`
/// projection fails to decode on Postgres; and a `CAST(etag AS TEXT)` strips the
/// blank padding that PHP emits on the wire (`FileDisplayResponse` sets the `ETag`
/// header from the padded `Preview::getEtag()` — verified 40 chars incl. trailing
/// `0x20`).  On Postgres we therefore project `RPAD(CAST(etag AS TEXT), 40)`, which
/// reproduces PHP's exact padded value byte-for-byte so a preview's `ETag` is
/// identical whether a hit (Rust) or a miss (PHP-FPM) served it — keeping
/// `If-None-Match` → `304` working across the hit/miss boundary.
pub async fn load_preview_rows(pool: &DbPool, prefix: &str, file_id: i64) -> Vec<PreviewRow> {
    let backend = backend_kind(pool);
    let sql = format!(
        "SELECT id, file_id, storage_id, width, height, mimetype_id, source_mimetype_id, \
         max, cropped, encrypted, {etag}, mtime, size, version_id \
         FROM {prefix}previews WHERE file_id = $1",
        etag = etag_projection(backend),
    );
    let rows = match sqlx::query(&sql).bind(file_id).fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => {
            // A query failure here must not be silent (CLAUDE.md hygiene rule 1) —
            // the caller treats an empty result as a cache miss and proxies to PHP.
            tracing::error!(error = %e, file_id = file_id, "load_preview_rows: SQL error");
            return Vec::new();
        }
    };
    let sqlite = backend == Backend::Sqlite;
    rows.iter()
        .map(|r| PreviewRow {
            id: r.get("id"),
            file_id: r.get("file_id"),
            storage_id: r.get("storage_id"),
            width: r.get::<i64, _>("width") as u32,
            height: r.get::<i64, _>("height") as u32,
            mimetype_id: r.get("mimetype_id"),
            source_mimetype_id: r.get("source_mimetype_id"),
            mtime: r.get("mtime"),
            size: r.get("size"),
            max: get_bool(r, "max", sqlite),
            cropped: get_bool(r, "cropped", sqlite),
            encrypted: get_bool(r, "encrypted", sqlite),
            etag: r.get("etag"),
            version_id: r.get("version_id"),
        })
        .collect()
}

/// Decode a boolean column: native `bool` everywhere except SQLite, where the `Any`
/// driver cannot decode a boolean and the column is an integer 0/1.
fn get_bool(r: &sqlx::any::AnyRow, col: &str, sqlite: bool) -> bool {
    if sqlite {
        r.get::<i64, _>(col) != 0
    } else {
        r.get::<bool, _>(col)
    }
}

/// The `oc_previews.etag` column width: `CHAR(40)` — `fixed => true, length => 40`
/// (`core/Migrations/Version33000Date20250819110529.php:61`).  PHP reads the blank
/// padding intact and emits it in the `ETag` header, so the Postgres projection must
/// re-pad to this exact width (see [`etag_projection`]).
const ETAG_CHAR_WIDTH: usize = 40;

/// The `Any` driver backend.  Only PostgreSQL (production, first-class) and SQLite
/// (test/legacy) are supported — see CLAUDE.md principle 6.  The dialect is fixed
/// for a running server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    Postgres,
    Sqlite,
}

/// The pool's backend from the `DbPool` enum variant (PHASE-22 T3.2): the
/// dialect is fixed for a running server, so the variant is the answer — no
/// connection round trip, no cache needed.
fn backend_kind(pool: &DbPool) -> Backend {
    if pool.is_postgres() {
        Backend::Postgres
    } else {
        Backend::Sqlite
    }
}

/// The `etag` column projection for a backend (see [`load_preview_rows`] for why
/// Postgres re-pads).  Postgres: `RPAD(CAST(etag AS TEXT), 40)` — yields a `text`
/// type the `Any` driver can decode and reproduces PHP's blank-padded value.
/// SQLite has no fixed-length `CHAR` padding, so it projects the bare column.
fn etag_projection(backend: Backend) -> String {
    match backend {
        Backend::Postgres => format!("RPAD(CAST(etag AS TEXT), {ETAG_CHAR_WIDTH}) AS etag"),
        Backend::Sqlite => "etag".to_string(),
    }
}

/// Output mimetype → file extension (`Preview::getExtension`): `png`/`gif`/`jpg`/
/// `webp`, defaulting to `png` for anything else (e.g. svg/tiff are converted to
/// png before storage).
pub fn extension_for_mime(output_mime: &str) -> &'static str {
    match output_mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// Build a preview file name (`Preview::getName`) — see [`PreviewRow::name`].
pub fn preview_name(
    version_id: i64,
    width: u32,
    height: u32,
    cropped: bool,
    max: bool,
    output_mime: &str,
) -> String {
    let mut name = String::new();
    if version_id > -1 {
        name.push_str(&version_id.to_string());
        name.push('-');
    }
    name.push_str(&width.to_string());
    name.push('-');
    name.push_str(&height.to_string());
    if cropped {
        name.push_str("-crop");
    }
    if max {
        name.push_str("-max");
    }
    name.push('.');
    name.push_str(extension_for_mime(output_mime));
    name
}

/// The md5-sharded directory prefix for a file id (`LocalPreviewStorage::constructPath`):
/// the first 7 hex chars of `md5(decimal file_id)`, each char a nested directory
/// (e.g. file_id `123` → `2/0/2/c/b/9/6`).
pub fn md5_shard(file_id: i64) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(file_id.to_string().as_bytes());
    let hex = hex::encode(digest); // lowercase
    let mut shard = String::with_capacity(13); // 7 chars + 6 slashes
    for (i, c) in hex.chars().take(7).enumerate() {
        if i > 0 {
            shard.push('/');
        }
        shard.push(c);
    }
    shard
}

/// The preview root directory: `{datadir}/appdata_{instanceid}/preview`
/// (`LocalPreviewStorage::getPreviewRootFolder`; `appdata_{instanceid}` is
/// `IRootFolder::getAppDataDirectoryName()`).
pub fn preview_root(datadir: &Path, instanceid: &str) -> PathBuf {
    datadir
        .join(format!("appdata_{instanceid}"))
        .join("preview")
}

/// Absolute byte path for a preview (`LocalPreviewStorage::constructPath`):
/// `{preview_root}/{md5_shard}/{file_id}/{name}`.
pub fn preview_byte_path(datadir: &Path, instanceid: &str, file_id: i64, name: &str) -> PathBuf {
    preview_root(datadir, instanceid)
        .join(md5_shard(file_id))
        .join(file_id.to_string())
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extension mapping (Preview::getExtension) ──────────────────────────

    #[test]
    fn extension_from_output_mime() {
        assert_eq!(extension_for_mime("image/png"), "png");
        assert_eq!(extension_for_mime("image/gif"), "gif");
        assert_eq!(extension_for_mime("image/jpeg"), "jpg");
        assert_eq!(extension_for_mime("image/webp"), "webp");
        // Converted / unknown outputs fall back to png.
        assert_eq!(extension_for_mime("image/svg+xml"), "png");
        assert_eq!(extension_for_mime("image/tiff"), "png");
        assert_eq!(extension_for_mime("application/octet-stream"), "png");
    }

    // ── file name (Preview::getName) — golden vectors from live PHP ────────

    #[test]
    fn preview_name_matches_php() {
        // (version_id, w, h, crop, max, mime) => expected name
        let cases: &[(i64, u32, u32, bool, bool, &str, &str)] = &[
            (-1, 1024, 768, false, false, "image/jpeg", "1024-768.jpg"),
            (-1, 256, 256, true, false, "image/png", "256-256-crop.png"),
            (
                -1,
                4096,
                4096,
                false,
                true,
                "image/jpeg",
                "4096-4096-max.jpg",
            ),
            (
                -1,
                512,
                512,
                true,
                true,
                "image/webp",
                "512-512-crop-max.webp",
            ),
            (-1, 64, 64, false, false, "image/gif", "64-64.gif"),
            // svg output is stored as png
            (
                -1,
                200,
                200,
                true,
                true,
                "image/svg+xml",
                "200-200-crop-max.png",
            ),
            // versioned (object-store/S3 — out of scope, but the naming is faithful)
            (
                1759276800,
                300,
                300,
                false,
                false,
                "image/jpeg",
                "1759276800-300-300.jpg",
            ),
        ];
        for &(v, w, h, crop, max, mime, exp) in cases {
            assert_eq!(
                preview_name(v, w, h, crop, max, mime),
                exp,
                "name({v},{w},{h},{crop},{max},{mime})"
            );
        }
    }

    // ── md5 shard (LocalPreviewStorage) — golden vectors from live PHP ─────

    #[test]
    fn md5_shard_matches_php() {
        assert_eq!(md5_shard(1), "c/4/c/a/4/2/3");
        assert_eq!(md5_shard(42), "a/1/d/0/c/6/e");
        assert_eq!(md5_shard(123), "2/0/2/c/b/9/6");
        assert_eq!(md5_shard(999999), "5/2/c/6/9/e/3");
        assert_eq!(md5_shard(3057), "c/1/5/0/2/a/e");
    }

    // ── full byte path ─────────────────────────────────────────────────────

    #[test]
    fn byte_path_layout() {
        let p = preview_byte_path(Path::new("/data"), "oc123", 123, "256-256-crop.png");
        assert_eq!(
            p,
            PathBuf::from("/data/appdata_oc123/preview/2/0/2/c/b/9/6/123/256-256-crop.png")
        );
    }

    #[test]
    fn preview_root_uses_appdata_instanceid() {
        assert_eq!(
            preview_root(Path::new("/var/nc/data"), "ocabc"),
            PathBuf::from("/var/nc/data/appdata_ocabc/preview")
        );
    }

    // ── row name / byte_path via PreviewRow ────────────────────────────────

    fn row(w: u32, h: u32, cropped: bool, max: bool, mime_id: i32, version_id: i64) -> PreviewRow {
        PreviewRow {
            id: 1,
            file_id: 123,
            storage_id: 1,
            width: w,
            height: h,
            mimetype_id: mime_id,
            source_mimetype_id: 9,
            mtime: 1_700_000_000,
            size: 4096,
            max,
            cropped,
            encrypted: false,
            etag: "abc".to_string(),
            version_id,
        }
    }

    #[test]
    fn row_name_and_path() {
        let r = row(4096, 4096, false, true, 5, -1);
        assert_eq!(r.name("image/jpeg"), "4096-4096-max.jpg");
        assert_eq!(
            r.byte_path(Path::new("/data"), "oc1", "image/jpeg"),
            PathBuf::from("/data/appdata_oc1/preview/2/0/2/c/b/9/6/123/4096-4096-max.jpg")
        );
    }

    // ── max / match selection (Generator) ──────────────────────────────────

    #[test]
    fn find_max_selects_max_row_for_version() {
        let rows = vec![
            row(256, 256, true, false, 5, -1),
            row(4096, 4096, false, true, 5, -1), // the max
            row(512, 512, false, false, 5, -1),
        ];
        let max = find_max(&rows, -1).expect("max row");
        assert!(max.max);
        assert_eq!((max.width, max.height), (4096, 4096));
        // A different version has no max row here.
        assert!(find_max(&rows, 999).is_none());
    }

    #[test]
    fn find_match_is_php_array_find() {
        let jpeg = 5;
        let rows = vec![
            row(4096, 4096, false, true, jpeg, -1),
            row(256, 256, true, false, jpeg, -1),
            row(256, 256, false, false, jpeg, -1),
            // same dims but a different output mime must NOT match
            row(256, 256, true, false, 7, -1),
        ];
        // (256,256,cropped,jpeg,-1) → the cropped jpeg variant.
        let m = find_match(&rows, 256, 256, true, jpeg, -1).expect("match");
        assert_eq!(
            (m.width, m.height, m.cropped, m.mimetype_id),
            (256, 256, true, jpeg)
        );
        // uncropped variant
        let m2 = find_match(&rows, 256, 256, false, jpeg, -1).expect("match");
        assert!(!m2.cropped && !m2.max);
        // no such size → miss
        assert!(find_match(&rows, 128, 128, false, jpeg, -1).is_none());
        // wrong output mime → miss (even though dims/crop match row index 3)
        assert!(find_match(&rows, 256, 256, true, 6, -1).is_none());
    }

    #[test]
    fn match_ignores_etag_mtime_source_mime() {
        // Rows differing only in etag/mtime/source_mimetype still match — staleness
        // is enforced by deletion-on-write (11.5), never at lookup.
        let mut a = row(512, 512, false, false, 5, -1);
        let mut b = a.clone();
        b.etag = "different".to_string();
        b.mtime = 1;
        b.source_mimetype_id = 42;
        for r in [&a, &b] {
            assert!(r.matches(512, 512, false, 5, -1));
        }
        a.cropped = true;
        assert!(!a.matches(512, 512, false, 5, -1));
    }

    // ── etag projection (dialect-aware CHAR(40) handling) ──────────────────

    #[test]
    fn etag_projection_pads_only_on_postgres() {
        // Postgres stores etag as CHAR(40) (bpchar); the Any driver cannot decode
        // bpchar, and a bare CAST strips the blank padding PHP emits on the wire —
        // so re-pad to the column width to reproduce PHP's exact value.
        assert_eq!(
            etag_projection(Backend::Postgres),
            "RPAD(CAST(etag AS TEXT), 40) AS etag"
        );
        // SQLite has no fixed-length padding → bare column.
        assert_eq!(etag_projection(Backend::Sqlite), "etag");
    }

    // ── DB round-trip (load_preview_rows against SQLite) ───────────────────

    #[tokio::test]
    async fn load_preview_rows_roundtrip() {
        // max_connections(1): a SQLite in-memory DB is per-connection, so a single
        // connection keeps the schema visible across the CREATE and the SELECT.
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        // oc_previews per REQ §9.10.  The boolean columns are declared INTEGER (not
        // BOOLEAN) because sqlx's `Any` driver cannot decode a SQLite boolean — on
        // SQLite they are read as integers (see `get_bool`); on PostgreSQL the
        // PHP-created columns are real BOOLEAN and are decoded natively as `bool`.
        sqlx::query(
            "CREATE TABLE oc_previews (
                id                 BIGINT  NOT NULL PRIMARY KEY,
                file_id            BIGINT  NOT NULL,
                storage_id         BIGINT  NOT NULL,
                old_file_id        BIGINT,
                location_id        BIGINT,
                width              INTEGER NOT NULL,
                height             INTEGER NOT NULL,
                mimetype_id        INTEGER NOT NULL,
                source_mimetype_id INTEGER NOT NULL,
                max                INTEGER NOT NULL DEFAULT 0,
                cropped            INTEGER NOT NULL DEFAULT 0,
                encrypted          INTEGER NOT NULL DEFAULT 0,
                etag               VARCHAR(40) NOT NULL DEFAULT '',
                mtime              INTEGER NOT NULL DEFAULT 0,
                size               INTEGER NOT NULL DEFAULT 0,
                version_id         BIGINT  NOT NULL DEFAULT -1
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let insert = "INSERT INTO oc_previews (id, file_id, storage_id, width, height, \
             mimetype_id, source_mimetype_id, max, cropped, encrypted, etag, mtime, size, version_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,0,$10,$11,$12,$13)";
        // max row for file 123
        sqlx::query(insert)
            .bind(101i64)
            .bind(123i64)
            .bind(1i64)
            .bind(4096i64)
            .bind(4096i64)
            .bind(5i32)
            .bind(9i32)
            .bind(true)
            .bind(false)
            .bind("srcetag")
            .bind(1_700_000_000i64)
            .bind(99_999i64)
            .bind(-1i64)
            .execute(&pool)
            .await
            .unwrap();
        // derived cropped 256x256 for file 123
        sqlx::query(insert)
            .bind(102i64)
            .bind(123i64)
            .bind(1i64)
            .bind(256i64)
            .bind(256i64)
            .bind(5i32)
            .bind(9i32)
            .bind(false)
            .bind(true)
            .bind("srcetag")
            .bind(1_700_000_001i64)
            .bind(4096i64)
            .bind(-1i64)
            .execute(&pool)
            .await
            .unwrap();
        // a different file's row — must not be returned
        sqlx::query(insert)
            .bind(103i64)
            .bind(999i64)
            .bind(1i64)
            .bind(64i64)
            .bind(64i64)
            .bind(5i32)
            .bind(9i32)
            .bind(true)
            .bind(false)
            .bind("x")
            .bind(1i64)
            .bind(1i64)
            .bind(-1i64)
            .execute(&pool)
            .await
            .unwrap();

        let rows = load_preview_rows(&pool, "oc_", 123).await;
        assert_eq!(rows.len(), 2, "only file 123's rows");

        let max = find_max(&rows, -1).expect("max row");
        assert_eq!(max.id, 101);
        assert!(max.max);
        assert_eq!((max.width, max.height), (4096, 4096));
        assert_eq!(max.etag, "srcetag"); // source etag at generation
        assert_eq!(max.mtime, 1_700_000_000); // generation timestamp
        assert_eq!(max.version_id, -1); // un-versioned (local disk)
        assert!(!max.encrypted);

        // the derived cropped 256x256 jpeg matches; the max row's output mime is the key
        let m = find_match(&rows, 256, 256, true, max.mimetype_id, -1).expect("derived variant");
        assert_eq!(m.id, 102);
        assert!(m.cropped && !m.max);

        // no such variant → cache miss
        assert!(find_match(&rows, 512, 512, false, max.mimetype_id, -1).is_none());
    }
}
