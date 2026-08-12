//! Canonicalization: natural-key id-bijection + equality-preserving masking
//! (Phases 16.4 minimal, 16.5 full).
//!
//! The SUT and Oracle use independent Postgres sequences/snowflakes, so the same
//! logical row has different ids that ripple through every FK. Rows are therefore
//! matched across sides by a **stable natural key**, not by id, in FK-dependency
//! (topological) order; every `id_pk`/`id_fk` is remapped to the referenced row's
//! natural key. Timestamps are masked to sentinels that preserve **equality
//! within a row** (a missed mtime bump is still caught), and volatile-but-
//! equality-meaningful values (etag) get per-table sentinels that preserve
//! equal-vs-distinct. Column classes come from `column_registry.yaml`; the
//! natural keys and FK graph are encoded here (they are a fixed property of the
//! Nextcloud schema).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result};

use crate::db::{Snapshot, TableData};

/// Column classification classes (see `column_registry.yaml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Stable,
    IdPk,
    IdFk,
    TimestampWall,
    VolatileValue,
    VolatileIndependent,
    Ignore,
}

impl Class {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "stable" => Class::Stable,
            "id_pk" => Class::IdPk,
            "id_fk" => Class::IdFk,
            "timestamp_wall" => Class::TimestampWall,
            "volatile_value" => Class::VolatileValue,
            "volatile_independent" => Class::VolatileIndependent,
            "ignore" => Class::Ignore,
            _ => return None,
        })
    }
}

/// Column classification registry loaded from `column_registry.yaml`.
#[derive(Debug, Default)]
pub struct Registry {
    tables: BTreeMap<String, BTreeMap<String, Class>>,
}

impl Registry {
    pub fn load(path: &str) -> Result<Self> {
        let mut buf = String::new();
        File::open(path)
            .with_context(|| format!("opening {path}"))?
            .read_to_string(&mut buf)?;
        let raw: BTreeMap<String, BTreeMap<String, String>> =
            serde_yaml::from_str(&buf).with_context(|| format!("parsing {path}"))?;
        let mut tables = BTreeMap::new();
        for (t, cols) in raw {
            let mut m = BTreeMap::new();
            for (c, class) in cols {
                let class = Class::parse(&class)
                    .with_context(|| format!("unknown class {class:?} for {t}.{c}"))?;
                m.insert(c, class);
            }
            tables.insert(t, m);
        }
        Ok(Self { tables })
    }

    pub fn class(&self, table: &str, col: &str) -> Option<Class> {
        self.tables.get(table).and_then(|m| m.get(col)).copied()
    }

    pub fn has_table(&self, table: &str) -> bool {
        self.tables.contains_key(table)
    }

    /// Override one column's class (Phase 16.9 scenario-level overrides: a
    /// client-dictated value such as `X-OC-Mtime` is deterministic, so the
    /// scenario may promote a masked timestamp column to `stable`).
    pub fn set_class(&mut self, table: &str, col: &str, class: Class) {
        self.tables
            .entry(table.to_string())
            .or_default()
            .insert(col.to_string(), class);
    }
}

/// A canonical row: attribute column -> canonical value, keyed by natural key.
pub type CanonRow = BTreeMap<String, String>;
/// A canonical table: natural key -> canonical row.
pub type CanonTable = BTreeMap<String, CanonRow>;

#[derive(Debug, Default)]
pub struct CanonicalSnapshot {
    pub tables: BTreeMap<String, CanonTable>,
}

/// One component of a table's natural key.
enum KeyPart {
    /// Use the column's raw value.
    Col(&'static str),
    /// The column is an FK; use the referenced row's natural key.
    FkCol(&'static str),
}

/// Static natural-key + PK spec for the diff-set tables, in FK-topological order.
struct TableSpec {
    name: &'static str,
    pk: &'static str,
    key: &'static [KeyPart],
}

const SPECS: &[TableSpec] = &[
    TableSpec {
        name: "oc_storages",
        pk: "numeric_id",
        key: &[KeyPart::Col("id")],
    },
    TableSpec {
        name: "oc_mimetypes",
        pk: "id",
        key: &[KeyPart::Col("mimetype")],
    },
    TableSpec {
        name: "oc_filecache",
        pk: "fileid",
        key: &[KeyPart::FkCol("storage"), KeyPart::Col("path")],
    },
    TableSpec {
        name: "oc_filecache_extended",
        pk: "fileid",
        key: &[KeyPart::FkCol("fileid")],
    },
    TableSpec {
        name: "oc_files_metadata",
        pk: "id",
        key: &[KeyPart::FkCol("file_id")],
    },
    TableSpec {
        name: "oc_properties",
        pk: "id",
        key: &[
            KeyPart::Col("userid"),
            KeyPart::Col("propertypath"),
            KeyPart::Col("propertyname"),
        ],
    },
    TableSpec {
        name: "oc_files_trash",
        pk: "auto_id",
        key: &[
            KeyPart::Col("user"),
            KeyPart::Col("location"),
            KeyPart::Col("type"),
            KeyPart::Col("mime"),
        ],
    },
    // One version row per (file) for the scenarios considered; a file with
    // multiple same-size versions would need `timestamp` in the key, but that is
    // volatile — revisit when a versioning scenario lands.
    TableSpec {
        name: "oc_files_versions",
        pk: "id",
        key: &[KeyPart::FkCol("file_id")],
    },
    TableSpec {
        name: "oc_preview_generation",
        pk: "id",
        key: &[KeyPart::FkCol("file_id")],
    },
    // Phase 16.11: preview row shape. `oc_preview_locations` first (oc_previews
    // references it via location_id).
    TableSpec {
        name: "oc_preview_locations",
        pk: "id",
        key: &[
            KeyPart::Col("bucket_name"),
            KeyPart::Col("object_store_name"),
        ],
    },
    // The key mirrors the live unique index `previews_file_uniq_idx`
    // (file_id, width, height, mimetype_id, cropped, version_id). `max` is
    // implied by the other components; `version_id` is -1 for current-file
    // previews (live-verified 2026-08-05). ids are snowflakes — only uniqueness
    // + natural-key matching matter (16.5 design).
    TableSpec {
        name: "oc_previews",
        pk: "id",
        key: &[
            KeyPart::FkCol("file_id"),
            KeyPart::Col("width"),
            KeyPart::Col("height"),
            KeyPart::FkCol("mimetype_id"),
            KeyPart::Col("cropped"),
            KeyPart::Col("version_id"),
        ],
    },
    TableSpec {
        name: "oc_preview_versions",
        pk: "id",
        key: &[KeyPart::FkCol("file_id"), KeyPart::Col("version")],
    },
    // Phase 16.7: `share_type` is part of the key — the user "admin" and group
    // "admin" would otherwise collide on `share_with` (TYPE_GROUP parent vs
    // TYPE_USERGROUP child rows share every other key component).
    TableSpec {
        name: "oc_share",
        pk: "id",
        key: &[
            KeyPart::Col("uid_owner"),
            KeyPart::Col("uid_initiator"),
            KeyPart::Col("share_type"),
            KeyPart::Col("item_type"),
            KeyPart::FkCol("item_source"),
            KeyPart::Col("share_with"),
            KeyPart::Col("file_target"),
        ],
    },
    TableSpec {
        name: "oc_vcategory",
        pk: "id",
        key: &[
            KeyPart::Col("uid"),
            KeyPart::Col("type"),
            KeyPart::Col("category"),
        ],
    },
    // Composite PK (categoryid, objid, type); leaf table — nothing references
    // it, so the empty `pk` (no single-column identity) is fine.
    TableSpec {
        name: "oc_vcategory_to_object",
        pk: "",
        key: &[
            KeyPart::FkCol("objid"),
            KeyPart::FkCol("categoryid"),
            KeyPart::Col("type"),
        ],
    },
];

/// Which table a given `id_fk` column references (referenced table, its PK column).
fn fk_reference(table: &str, col: &str) -> Option<(&'static str, &'static str)> {
    Some(match (table, col) {
        ("oc_filecache", "storage") => ("oc_storages", "numeric_id"),
        ("oc_filecache", "mimetype") => ("oc_mimetypes", "id"),
        ("oc_filecache", "mimepart") => ("oc_mimetypes", "id"),
        ("oc_filecache", "parent") => ("oc_filecache", "fileid"),
        ("oc_filecache_extended", "fileid") => ("oc_filecache", "fileid"),
        ("oc_files_metadata", "file_id") => ("oc_filecache", "fileid"),
        ("oc_files_versions", "file_id") => ("oc_filecache", "fileid"),
        ("oc_files_versions", "mimetype") => ("oc_mimetypes", "id"),
        ("oc_preview_generation", "file_id") => ("oc_filecache", "fileid"),
        ("oc_previews", "file_id") => ("oc_filecache", "fileid"),
        ("oc_previews", "storage_id") => ("oc_storages", "numeric_id"),
        ("oc_previews", "mimetype_id") => ("oc_mimetypes", "id"),
        ("oc_previews", "source_mimetype_id") => ("oc_mimetypes", "id"),
        ("oc_previews", "old_file_id") => ("oc_filecache", "fileid"),
        ("oc_previews", "location_id") => ("oc_preview_locations", "id"),
        ("oc_preview_versions", "file_id") => ("oc_filecache", "fileid"),
        ("oc_share", "item_source") => ("oc_filecache", "fileid"),
        ("oc_share", "file_source") => ("oc_filecache", "fileid"),
        ("oc_share", "parent") => ("oc_share", "id"),
        ("oc_vcategory_to_object", "objid") => ("oc_filecache", "fileid"),
        ("oc_vcategory_to_object", "categoryid") => ("oc_vcategory", "id"),
        _ => return None,
    })
}

pub struct Canonicalizer {
    pub registry: Registry,
}

impl Canonicalizer {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Canonicalize a whole-instance snapshot.
    pub fn canonicalize(&self, snap: &Snapshot) -> Result<CanonicalSnapshot> {
        // pk value -> natural key, per table. Built in topological order so an
        // FK always resolves against an already-built map.
        let mut pk_maps: HashMap<String, HashMap<String, String>> = HashMap::new();
        // Raw rows keyed by natural key (values still raw), per table.
        let mut raw: BTreeMap<String, BTreeMap<String, BTreeMap<String, Option<String>>>> =
            BTreeMap::new();

        // Pass 1: configured diff-set tables, in topological order.
        for spec in SPECS {
            let Some(td) = snap.tables.get(spec.name) else {
                continue;
            };
            let col_idx = column_index(td);
            let mut pk_map: HashMap<String, String> = HashMap::new();
            let mut by_key: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();

            for row in &td.rows {
                let nk = self.natural_key(spec, td, &col_idx, row, &pk_maps)?;
                if let Some(pk_val) = cell(row, &col_idx, spec.pk) {
                    pk_map.insert(pk_val, nk.clone());
                }
                let mut attrs = BTreeMap::new();
                for c in &td.columns {
                    attrs.insert(c.clone(), cell(row, &col_idx, c));
                }
                by_key.insert(nk, attrs);
            }
            pk_maps.insert(spec.name.to_string(), pk_map);
            raw.insert(spec.name.to_string(), by_key);
        }

        // Pass 1b: any other snapshotted table is keyed by its raw content and
        // carried verbatim — untouched tables yield an empty delta; a touched but
        // unclassified table surfaces raw and loudly, prompting classification.
        for (name, td) in &snap.tables {
            if raw.contains_key(name) {
                continue;
            }
            let mut by_key: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();
            for row in &td.rows {
                let key = row
                    .iter()
                    .map(|v| v.clone().unwrap_or_else(|| "\u{0}".into()))
                    .collect::<Vec<_>>()
                    .join("\u{1}");
                let mut attrs = BTreeMap::new();
                for (i, c) in td.columns.iter().enumerate() {
                    attrs.insert(c.clone(), row[i].clone());
                }
                by_key.insert(key, attrs);
            }
            raw.insert(name.clone(), by_key);
        }

        // Pass 2: canonicalize attribute values structurally (FK remap). Volatile
        // columns (`timestamp_wall`, `volatile_value`) are left RAW here and masked
        // in `delta::normalize_delta` — sentinel assignment must happen over the
        // *delta* (whose natural-key sets match when behavior matches), not the
        // full snapshot (whose baseline rows differ between sides).
        let mut out = CanonicalSnapshot::default();
        for (name, by_key) in &raw {
            let spec = SPECS.iter().find(|s| s.name == name);
            let mut table = CanonTable::new();

            for (nk, attrs) in by_key {
                let mut crow = CanonRow::new();
                // For oc_filecache rows in the trashbin/versions subtrees the
                // path carries a wall-clock suffix (stripped above/below); the
                // `name` column (no subtree prefix) and the `path_hash` (md5 of
                // the UNstripped path) need the same row-context handling.
                let volatile_subtree = name == "oc_filecache"
                    && attrs
                        .get("path")
                        .and_then(|p| p.as_deref())
                        .map(|p| strip_volatile_path_suffixes(p) != p)
                        .unwrap_or(false);
                for (col, val) in attrs {
                    let class = self.registry.class(name, col).unwrap_or(Class::Stable);
                    // The PK column is fully represented by the natural key.
                    if let Some(s) = spec {
                        if s.pk == col && matches!(class, Class::IdPk) {
                            continue;
                        }
                    }
                    let canon = match class {
                        Class::IdPk => continue, // identity; captured by natural key
                        Class::Ignore => continue,
                        Class::Stable => {
                            if volatile_subtree && col == "name" {
                                strip_ts_suffixes(val.as_deref().unwrap_or(""))
                            } else if volatile_subtree && col == "path_hash" {
                                // md5 of the volatile unstripped path — not comparable.
                                "PHV".to_string()
                            } else {
                                self.stable_value(name, col, val)
                            }
                        }
                        // Left raw; masked in normalize_delta.
                        Class::TimestampWall | Class::VolatileValue => {
                            val.clone().unwrap_or_else(|| "\u{0}".to_string())
                        }
                        Class::IdFk => match fk_reference(name, col) {
                            Some((ref_table, _)) => match val {
                                Some(v) => pk_maps
                                    .get(ref_table)
                                    .and_then(|m| m.get(v))
                                    .cloned()
                                    .unwrap_or_else(|| format!("?{ref_table}:{v}")),
                                None => "\u{0}".to_string(),
                            },
                            // Unclassified FK: keep raw (will surface divergence).
                            None => val.clone().unwrap_or_default(),
                        },
                        Class::VolatileIndependent => "VI".to_string(),
                    };
                    crow.insert(col.clone(), canon);
                }
                table.insert(nk.clone(), crow);
            }
            out.tables.insert(name.clone(), table);
        }
        Ok(out)
    }

    /// Compute a row's natural key.
    fn natural_key(
        &self,
        spec: &TableSpec,
        td: &TableData,
        col_idx: &HashMap<String, usize>,
        row: &[Option<String>],
        pk_maps: &HashMap<String, HashMap<String, String>>,
    ) -> Result<String> {
        let mut parts = Vec::new();
        for part in spec.key {
            match part {
                KeyPart::Col(c) => {
                    let v = cell(row, col_idx, c).unwrap_or_else(|| "\u{0}".into());
                    // The filecache path of a trash/version entry carries a
                    // wall-clock suffix that differs between sides by replay
                    // timing; the natural key must use the stripped form.
                    let v = if spec.name == "oc_filecache" && *c == "path" {
                        strip_volatile_path_suffixes(&v)
                    } else {
                        v
                    };
                    parts.push(v);
                }
                KeyPart::FkCol(c) => {
                    let (ref_table, _) = fk_reference(spec.name, c)
                        .with_context(|| format!("no FK target for {}.{}", spec.name, c))?;
                    let v = cell(row, col_idx, c);
                    let canon = match v {
                        Some(v) => pk_maps
                            .get(ref_table)
                            .and_then(|m| m.get(&v))
                            .cloned()
                            .unwrap_or_else(|| format!("?{ref_table}:{v}")),
                        None => "\u{0}".to_string(),
                    };
                    parts.push(canon);
                }
            }
        }
        // Include the table name so keys are unambiguous in reports.
        let _ = td; // (columns are accessed via col_idx)
        Ok(parts.join("\u{1}"))
    }

    /// Stable-value handling, incl. the table-specific volatile-suffix
    /// transforms:
    /// - `oc_files_trash.id` is "{filename}.d{deletion-timestamp}" — keep the
    ///   filename, drop the volatile timestamp suffix (it is not classified
    ///   volatile_independent because the filename identity must be preserved).
    /// - `oc_filecache.path` / `.name` under `files_trashbin/` and
    ///   `files_versions/` carry `.d{deletion-ts}` / `.v{mtime}` suffixes whose
    ///   timestamps are wall-clock of the replay (the two sides replay
    ///   sequentially and can straddle a second boundary) — strip them.
    ///   `path_hash` is masked in `canonicalize` when the path was transformed.
    fn stable_value(&self, table: &str, col: &str, val: &Option<String>) -> String {
        let v = match val {
            None => return "\u{0}".to_string(),
            Some(v) => v.clone(),
        };
        if table == "oc_files_trash" && col == "id" {
            if let Some(idx) = v.rfind(".d") {
                let suffix = &v[idx + 2..];
                if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                    return v[..idx].to_string();
                }
            }
        }
        if table == "oc_filecache" && (col == "path" || col == "name") {
            return strip_volatile_path_suffixes(&v);
        }
        v
    }
}

/// Strip trailing `.d{digits}` then `.v{digits}` suffixes (a trashed version
/// file is `{name}.v{mtime}.d{deletion-ts}` — the `.d` goes first).
fn strip_ts_suffixes(p: &str) -> String {
    let mut p = p.to_string();
    for marker in [".d", ".v"] {
        if let Some(idx) = p.rfind(marker) {
            let suffix = &p[idx + 2..];
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                p.truncate(idx);
            }
        }
    }
    p
}

/// Strip wall-clock `.d{deletion-timestamp}` and `.v{version-mtime}` suffixes
/// from paths in the trashbin and versions subtrees. Both replays run
/// sequentially, so these timestamps can differ by a second between sides; the
/// identity is the original filename/location, not the instant of deletion.
/// (Documented trade-off, same as the `oc_files_trash.id` strip: a user file
/// literally named `*.d{digits}` inside those subtrees would be over-stripped.)
fn strip_volatile_path_suffixes(path: &str) -> String {
    if path.starts_with("files_trashbin/") || path.starts_with("files_versions/") {
        strip_ts_suffixes(path)
    } else {
        path.to_string()
    }
}

fn column_index(td: &TableData) -> HashMap<String, usize> {
    td.columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.clone(), i))
        .collect()
}

fn cell(row: &[Option<String>], col_idx: &HashMap<String, usize>, col: &str) -> Option<String> {
    col_idx
        .get(col)
        .and_then(|&i| row.get(i).cloned())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Snapshot, TableData};
    use crate::delta;

    fn registry() -> Registry {
        Registry::load(&format!(
            "{}/column_registry.yaml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("column_registry.yaml loads")
    }

    fn storages_table(numeric_id: &str) -> TableData {
        TableData {
            columns: vec![
                "numeric_id".into(),
                "id".into(),
                "available".into(),
                "last_checked".into(),
            ],
            rows: vec![vec![
                Some(numeric_id.into()),
                Some("home::admin".into()),
                Some("1".into()),
                Some("1700".into()),
            ]],
        }
    }

    /// filecache row: [fileid, storage, path, name, size, mtime, storage_mtime, etag, permissions]
    fn filecache_table(rows: Vec<[&str; 9]>) -> TableData {
        TableData {
            columns: [
                "fileid",
                "storage",
                "path",
                "name",
                "size",
                "mtime",
                "storage_mtime",
                "etag",
                "permissions",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            rows: rows
                .into_iter()
                .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
                .collect(),
        }
    }

    fn snap_with(storage_nid: &str, fc: Vec<[&str; 9]>) -> Snapshot {
        let mut s = Snapshot::default();
        s.tables
            .insert("oc_storages".into(), storages_table(storage_nid));
        if !fc.is_empty() {
            s.tables.insert("oc_filecache".into(), filecache_table(fc));
        }
        s
    }

    #[test]
    fn id_offset_hidden() {
        let canon = Canonicalizer::new(registry());
        // Identical logical state, different id sequences + FK ripple.
        let a = snap_with(
            "1",
            vec![["10", "1", "files/x", "x", "5", "100", "100", "e1", "27"]],
        );
        let b = snap_with(
            "99",
            vec![["500", "99", "files/x", "x", "5", "100", "100", "e1", "27"]],
        );
        let ca = canon.canonicalize(&a).unwrap();
        let cb = canon.canonicalize(&b).unwrap();
        assert_eq!(
            ca.tables, cb.tables,
            "id offset + FK ripple must be hidden by the natural-key bijection"
        );
    }

    #[test]
    fn natural_key_mismatch_reported() {
        let canon = Canonicalizer::new(registry());
        let with = snap_with(
            "1",
            vec![["10", "1", "files/x", "x", "5", "100", "100", "e1", "27"]],
        );
        let without = snap_with("1", vec![]);
        let c_with = canon.canonicalize(&with).unwrap();
        let c_without = canon.canonicalize(&without).unwrap();
        assert_ne!(
            c_with.tables, c_without.tables,
            "a row present on one side only must not be masked"
        );
    }

    fn mimetypes_table(rows: Vec<[&str; 2]>) -> TableData {
        TableData {
            columns: vec!["id".into(), "mimetype".into()],
            rows: rows
                .into_iter()
                .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
                .collect(),
        }
    }

    /// previews row (live column order): [id, file_id, storage_id, old_file_id,
    /// location_id, width, height, mimetype_id, source_mimetype_id, max,
    /// cropped, encrypted, etag, mtime, size, version_id]
    fn previews_table(rows: Vec<[Option<&str>; 16]>) -> TableData {
        TableData {
            columns: [
                "id",
                "file_id",
                "storage_id",
                "old_file_id",
                "location_id",
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
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            rows: rows
                .into_iter()
                .map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect())
                .collect(),
        }
    }

    /// Phase 16.11: snowflake ids + id offsets on `oc_previews` (and the FK
    /// ripple through filecache/mimetypes/storages) must be hidden by the
    /// natural-key bijection; a shape difference (width) must not.
    #[test]
    fn preview_snowflake_offset_hidden_shape_mismatch_reported() {
        let canon = Canonicalizer::new(registry());
        let preview_row = |id: &'static str,
                           fid: &'static str,
                           sid: &'static str,
                           mid: &'static str,
                           w: &'static str| {
            [
                Some(id),
                Some(fid),
                Some(sid),
                None,
                None,
                Some(w),
                Some("200"),
                Some(mid),
                Some(mid),
                Some("t"),
                Some("f"),
                Some("f"),
                Some("e1"),
                Some("1700"),
                Some("828"),
                Some("-1"),
            ]
        };
        let snap = |nid: &'static str,
                    fid: &'static str,
                    mid: &'static str,
                    pid: &'static str,
                    w: &'static str| {
            let mut s = snap_with(
                nid,
                vec![
                    ([
                        fid,
                        nid,
                        "files/img.png",
                        "img.png",
                        "1270",
                        "1700",
                        "1700",
                        "e1",
                        "27",
                    ]),
                ],
            );
            s.tables.insert(
                "oc_mimetypes".into(),
                mimetypes_table(vec![[mid, "image/png"]]),
            );
            s.tables.insert(
                "oc_previews".into(),
                previews_table(vec![preview_row(pid, fid, nid, mid, w)]),
            );
            s
        };

        // Same logical preview, different snowflake + FK ids across sides.
        let a = snap("1", "10", "5", "114114976154415104", "320");
        let b = snap("99", "500", "77", "114213092589088768", "320");
        assert_eq!(
            canon.canonicalize(&a).unwrap().tables,
            canon.canonicalize(&b).unwrap().tables,
            "snowflake ids and FK offsets must be hidden on oc_previews"
        );

        // A real shape difference (width) must surface.
        let c = snap("1", "10", "5", "114114976154415104", "256");
        assert_ne!(
            canon.canonicalize(&a).unwrap().tables,
            canon.canonicalize(&c).unwrap().tables,
            "a width difference is a shape divergence and must not be masked"
        );
    }

    /// before(empty) -> after(one filecache row) delta, normalized.
    fn put_delta(mtime: &str, smtime: &str) -> delta::Delta {
        let canon = Canonicalizer::new(registry());
        let before = snap_with("1", vec![]);
        let after = snap_with(
            "1",
            vec![["10", "1", "files/x", "x", "5", mtime, smtime, "e1", "27"]],
        );
        let cb = canon.canonicalize(&before).unwrap();
        let ca = canon.canonicalize(&after).unwrap();
        delta::normalize_delta(delta::delta(&cb, &ca), &canon.registry)
    }

    #[test]
    fn timestamp_equality_preserved() {
        let d1 = put_delta("100", "100"); // mtime == storage_mtime
        let d2 = put_delta("999", "999"); // equal pair, different absolute value
        assert_eq!(
            d1, d2,
            "equal timestamp pairs must canonicalize identically"
        );
        let d3 = put_delta("999", "1000"); // mtime != storage_mtime
        assert_ne!(
            d1, d3,
            "a broken timestamp-equality relationship must still be caught"
        );
    }

    /// Two files sharing an etag on both sides -> same sentinel; distinct -> not.
    fn etag_delta(etag_a: &str, etag_b: &str) -> delta::Delta {
        let canon = Canonicalizer::new(registry());
        let before = snap_with("1", vec![]);
        let after = snap_with(
            "1",
            vec![
                ["10", "1", "files/a", "a", "5", "100", "100", etag_a, "27"],
                ["11", "1", "files/b", "b", "5", "100", "100", etag_b, "27"],
            ],
        );
        let cb = canon.canonicalize(&before).unwrap();
        let ca = canon.canonicalize(&after).unwrap();
        delta::normalize_delta(delta::delta(&cb, &ca), &canon.registry)
    }

    #[test]
    fn volatile_equality_preserved() {
        let shared_a = etag_delta("X", "X"); // equal etags
        let shared_b = etag_delta("Y", "Y"); // equal etags, different absolute
        assert_eq!(
            shared_a, shared_b,
            "equal etags must stay equal after masking"
        );
        let distinct = etag_delta("Y", "Z"); // distinct etags
        assert_ne!(
            shared_a, distinct,
            "distinct etags must stay distinct after masking"
        );
    }

    #[test]
    fn divergences_report_column_level() {
        // Same structure, same masked etag -> no divergence.
        let a = etag_delta("X", "X");
        assert!(delta::divergences(&a, &a).is_empty());

        // A structural difference (one file only) -> a row divergence with the
        // full column list.
        let canon = Canonicalizer::new(registry());
        let before = snap_with("1", vec![]);
        let after = snap_with(
            "1",
            vec![["10", "1", "files/a", "a", "5", "100", "100", "X", "27"]],
        );
        let cb = canon.canonicalize(&before).unwrap();
        let ca = canon.canonicalize(&after).unwrap();
        let one = delta::normalize_delta(delta::delta(&cb, &ca), &canon.registry);
        let two = etag_delta("X", "X");
        let divs = delta::divergences(&one, &two);
        assert!(
            divs.iter()
                .any(|d| d.table == "oc_filecache" && d.key == "home::admin\u{1}files/b"),
            "the single-file delta must surface the row divergence: {divs:?}"
        );
    }

    #[test]
    fn inventory_matches_known_and_rejects_unlisted() {
        use crate::divergences::{DivergenceRecord, Inventory};
        // Two records: the root-size (accepted) and the boundary noise.
        let recs = vec![
            DivergenceRecord {
                id: "home-root-size".into(),
                why: "test".into(),
                status: "accepted".into(),
                revisit: None,
                scenarios: vec!["10_put_get".into()],
                table: "oc_filecache".into(),
                key: "home::admin".into(),
                columns: vec!["size".into()],
            },
            DivergenceRecord {
                id: "replay-second-boundary".into(),
                why: "test".into(),
                status: "noise".into(),
                revisit: None,
                scenarios: vec![],
                table: "oc_filecache".into(),
                key: "".into(),
                columns: vec!["etag".into(), "storage_mtime".into(), "mtime".into()],
            },
        ];
        let inv = Inventory { records: recs };
        let divs = vec![
            crate::delta::Divergence {
                table: "oc_filecache".into(),
                key: "home::admin".into(),
                columns: vec!["size".into()],
            },
            crate::delta::Divergence {
                table: "oc_filecache".into(),
                key: "home::adminfiles".into(),
                columns: vec!["storage_mtime".into()],
            },
            crate::delta::Divergence {
                table: "oc_filecache".into(),
                key: "home::adminfiles/hello.txt".into(),
                columns: vec!["etag".into(), "size".into()],
            },
        ];
        let (known, unlisted) = inv.match_run("10_put_get", &divs);
        assert_eq!(known.len(), 2, "root-size + noise must match");
        assert_eq!(
            unlisted.len(),
            1,
            "the size column is not covered by the noise record"
        );
        assert_eq!(unlisted[0].key, "home::adminfiles/hello.txt");

        // Same divergences in a scenario the root-size record does not list.
        let (known2, _) = inv.match_run("99_other", &divs);
        assert_eq!(
            known2.len(),
            1,
            "only the scenario-less noise record matches"
        );
    }

    #[test]
    fn registry_coverage() {
        let reg = registry();
        for t in [
            "oc_filecache",
            "oc_storages",
            "oc_mimetypes",
            "oc_filecache_extended",
            "oc_files_metadata",
            "oc_files_trash",
            "oc_properties",
            "oc_files_versions",
            "oc_preview_generation",
            "oc_previews",
            "oc_preview_locations",
            "oc_preview_versions",
            "oc_share",
            "oc_vcategory",
            "oc_vcategory_to_object",
        ] {
            assert!(reg.has_table(t), "registry missing diff-set table {t}");
        }
    }

    /// oc_share fixture: columns in live-schema order (subset). Row shape:
    /// [id, share_type, share_with, uid_owner, uid_initiator, parent, item_type,
    ///  item_source, file_source, file_target, permissions, stime, accepted, token]
    fn share_table(rows: Vec<[Option<&str>; 14]>) -> TableData {
        TableData {
            columns: [
                "id",
                "share_type",
                "share_with",
                "uid_owner",
                "uid_initiator",
                "parent",
                "item_type",
                "item_source",
                "file_source",
                "file_target",
                "permissions",
                "stime",
                "accepted",
                "token",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            rows: rows
                .into_iter()
                .map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect())
                .collect(),
        }
    }

    /// A group share (TYPE_GROUP parent + TYPE_USERGROUP child) at two different
    /// id offsets (filecache id, share ids) must canonicalize identically — the
    /// child's self-referential `parent` FK remaps through the share bijection.
    #[test]
    fn share_id_offset_and_parent_remap_hidden() {
        let canon = Canonicalizer::new(registry());
        let media = |fid: &'static str, nid: &'static str| -> [&'static str; 9] {
            [
                fid,
                nid,
                "files/Media",
                "Media",
                "100",
                "100",
                "100",
                "e1",
                "31",
            ]
        };
        let group_and_child = |gid: &'static str,
                               cid: &'static str,
                               fid: &'static str|
         -> Vec<[Option<&'static str>; 14]> {
            vec![
                [
                    Some(gid),
                    Some("1"),
                    Some("admin"),
                    Some("admin"),
                    Some("admin"),
                    None,
                    Some("folder"),
                    Some(fid),
                    Some(fid),
                    Some("/Media"),
                    Some("31"),
                    Some("100"),
                    Some("0"),
                    None,
                ],
                [
                    Some(cid),
                    Some("2"),
                    Some("admin"),
                    Some("admin"),
                    Some("admin"),
                    Some(gid),
                    Some("folder"),
                    Some(fid),
                    Some(fid),
                    Some("/Media"),
                    Some("31"),
                    Some("100"),
                    Some("1"),
                    None,
                ],
            ]
        };

        let mut a = snap_with("1", vec![media("3", "1")]);
        a.tables.insert(
            "oc_share".into(),
            share_table(group_and_child("1", "2", "3")),
        );
        let mut b = snap_with("99", vec![media("55", "99")]);
        b.tables.insert(
            "oc_share".into(),
            share_table(group_and_child("41", "42", "55")),
        );

        let ca = canon.canonicalize(&a).unwrap();
        let cb = canon.canonicalize(&b).unwrap();
        assert_eq!(
            ca.tables, cb.tables,
            "share id offset + parent self-FK ripple must be hidden by the bijection"
        );

        // The group parent and its USERGROUP child must land on DISTINCT natural
        // keys even though share_with is the same string ("admin" user vs group).
        let shares = &ca.tables["oc_share"];
        assert_eq!(
            shares.len(),
            2,
            "group parent + usergroup child are two rows"
        );
    }

    /// Trashed/versioned filecache rows whose paths differ only in the
    /// wall-clock `.d{ts}` / `.v{ts}` suffixes must canonicalize identically;
    /// rows outside those subtrees keep their suffixes verbatim.
    #[test]
    fn trash_volatile_suffix_stripped() {
        let canon = Canonicalizer::new(registry());
        let row = |fid: &'static str, path: &'static str, ph: &'static str| -> [&'static str; 10] {
            [fid, "1", path, "ignored", "5", "100", "100", "e1", "27", ph]
        };
        let snap = |rows: Vec<[&'static str; 10]>| {
            let td = TableData {
                columns: [
                    "fileid",
                    "storage",
                    "path",
                    "name",
                    "size",
                    "mtime",
                    "storage_mtime",
                    "etag",
                    "permissions",
                    "path_hash",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
                rows: rows
                    .into_iter()
                    .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
                    .collect(),
            };
            let mut s = snap_with("1", vec![]);
            s.tables.insert("oc_filecache".into(), td);
            s
        };

        // Same logical trash row, deletion seconds differ between sides.
        let a = snap(vec![
            row("10", "files_trashbin/files/hello.txt.d1000", "aaa"),
            row("11", "files_trashbin/versions/hello.txt.v900.d1000", "bbb"),
        ]);
        let b = snap(vec![
            row("10", "files_trashbin/files/hello.txt.d1001", "ccc"),
            row("11", "files_trashbin/versions/hello.txt.v901.d1001", "ddd"),
        ]);
        assert_eq!(
            canon.canonicalize(&a).unwrap().tables,
            canon.canonicalize(&b).unwrap().tables,
            "wall-clock trash/version suffixes must be hidden"
        );

        // A regular file whose name merely looks suffixed is NOT stripped.
        let c = snap(vec![row("10", "files/report.d20240101", "aaa")]);
        let d = snap(vec![row("10", "files/report.d20240102", "aaa")]);
        assert_ne!(
            canon.canonicalize(&c).unwrap().tables,
            canon.canonicalize(&d).unwrap().tables,
            "suffixes outside trash/version subtrees are stable content"
        );
    }

    /// A child whose `parent` points at a different share must NOT canonicalize
    /// to the same row — the self-FK remap is part of the compared content.
    #[test]
    fn share_wrong_parent_reported() {
        let canon = Canonicalizer::new(registry());
        let rows = |child_parent: &'static str| -> Vec<[Option<&'static str>; 14]> {
            vec![
                [
                    Some("1"),
                    Some("1"),
                    Some("admin"),
                    Some("admin"),
                    Some("admin"),
                    None,
                    Some("folder"),
                    Some("3"),
                    Some("3"),
                    Some("/Media"),
                    Some("31"),
                    Some("100"),
                    Some("0"),
                    None,
                ],
                // Second group share (a different natural key) the child could
                // wrongly point at.
                [
                    Some("9"),
                    Some("1"),
                    Some("admin"),
                    Some("admin"),
                    Some("admin"),
                    None,
                    Some("folder"),
                    Some("3"),
                    Some("3"),
                    Some("/Other"),
                    Some("31"),
                    Some("100"),
                    Some("0"),
                    None,
                ],
                [
                    Some("2"),
                    Some("2"),
                    Some("admin"),
                    Some("admin"),
                    Some("admin"),
                    Some(child_parent),
                    Some("folder"),
                    Some("3"),
                    Some("3"),
                    Some("/Media"),
                    Some("31"),
                    Some("100"),
                    Some("1"),
                    None,
                ],
            ]
        };
        let mut a = snap_with("1", vec![]);
        a.tables.insert("oc_share".into(), share_table(rows("1")));
        let mut b = snap_with("1", vec![]);
        b.tables.insert("oc_share".into(), share_table(rows("9")));
        let ca = canon.canonicalize(&a).unwrap();
        let cb = canon.canonicalize(&b).unwrap();
        assert_ne!(ca.tables, cb.tables, "a wrong parent FK must not be masked");
    }
}
