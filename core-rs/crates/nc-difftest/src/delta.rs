//! Snapshot → Delta (Phase 16.4) + volatile normalization.
//!
//! A `Delta` is the set of added / removed / changed canonical rows per table
//! between the before and after snapshots of ONE side. Comparing deltas (not
//! absolute state) cancels install-time differences (instanceid, auto-id
//! watermarks, baseline rows).
//!
//! Volatile columns (`timestamp_wall`, `volatile_value`) arrive RAW from the
//! structural canonicalizer and are masked here by `normalize_delta`. Masking is
//! done over the *delta* — whose natural-key sets match when behavior matches —
//! and sentinels are assigned in natural-key order, so the assignment is
//! identical on both sides. Because sentinels preserve **equality**, a missed
//! etag/mtime bump (value unchanged on one side, bumped on the other) still shows
//! up as a different sentinel pattern.

use std::collections::{BTreeMap, HashMap};

use crate::canonicalize::{CanonRow, CanonicalSnapshot, Class, Registry};

#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Added(CanonRow),
    Removed(CanonRow),
    Changed { before: CanonRow, after: CanonRow },
}

/// natural key -> change, for one table.
pub type TableDelta = BTreeMap<String, RowChange>;
/// table -> its changes.
pub type Delta = BTreeMap<String, TableDelta>;

/// Compute the delta between two structural canonical snapshots of one side.
pub fn delta(before: &CanonicalSnapshot, after: &CanonicalSnapshot) -> Delta {
    let mut d = Delta::new();
    let empty = BTreeMap::new();

    let mut names: Vec<&String> = before.tables.keys().collect();
    for k in after.tables.keys() {
        if !before.tables.contains_key(k) {
            names.push(k);
        }
    }
    names.sort();

    for name in names {
        let b = before.tables.get(name).unwrap_or(&empty);
        let a = after.tables.get(name).unwrap_or(&empty);

        let mut keys: Vec<&String> = b.keys().collect();
        for k in a.keys() {
            if !b.contains_key(k) {
                keys.push(k);
            }
        }
        keys.sort();

        let mut td = TableDelta::new();
        for k in keys {
            match (b.get(k), a.get(k)) {
                (None, Some(ar)) => {
                    td.insert(k.clone(), RowChange::Added(ar.clone()));
                }
                (Some(br), None) => {
                    td.insert(k.clone(), RowChange::Removed(br.clone()));
                }
                (Some(br), Some(ar)) => {
                    if br != ar {
                        td.insert(
                            k.clone(),
                            RowChange::Changed {
                                before: br.clone(),
                                after: ar.clone(),
                            },
                        );
                    }
                }
                (None, None) => {}
            }
        }
        if !td.is_empty() {
            d.insert(name.clone(), td);
        }
    }
    d
}

/// One structured divergence between the two sides: a row (canonical natural
/// key) in a table with the divergent columns listed.  A row with an empty
/// `columns` list diverges structurally (e.g. the change type differs: Added
/// on one side, Changed on the other).
#[derive(Debug, Clone, PartialEq)]
pub struct Divergence {
    pub table: String,
    pub key: String,
    pub columns: Vec<String>,
}

/// Extract the column-level divergences between two masked deltas.
///
/// Each row is compared state-by-state: Added has only an after-state, Removed
/// only a before-state, Changed has both.  A column diverges when the sides'
/// states disagree on it (a value on one side and a different value — or no
/// value at all — on the other).  The change TYPE is compared separately: a
/// row whose type differs (Added vs Changed, Added vs none, …) diverges even
/// when every shared column agrees (e.g. the `uploads` row added on one side
/// and only etag-bumped on the other — the accumulated-state residue).
pub fn divergences(sut: &Delta, oracle: &Delta) -> Vec<Divergence> {
    let mut out = Vec::new();
    let mut tables: Vec<&String> = sut.keys().collect();
    for t in oracle.keys() {
        if !sut.contains_key(t) {
            tables.push(t);
        }
    }
    tables.sort();
    for table in tables {
        let empty_s = TableDelta::new();
        let empty_o = TableDelta::new();
        let s = sut.get(table).unwrap_or(&empty_s);
        let o = oracle.get(table).unwrap_or(&empty_o);
        let mut keys: Vec<&String> = s.keys().collect();
        for k in o.keys() {
            if !s.contains_key(k) {
                keys.push(k);
            }
        }
        keys.sort();
        for key in keys {
            let (sb, sa) = row_states(s.get(key));
            let (ob, oa) = row_states(o.get(key));
            let cols = differing_columns(sb, sa, ob, oa);
            if !cols.is_empty() || change_type(s.get(key)) != change_type(o.get(key)) {
                out.push(Divergence {
                    table: table.clone(),
                    key: key.clone(),
                    columns: cols,
                });
            }
        }
    }
    out
}

/// The (before, after) states of a row on one side; both `None` when absent.
fn row_states(c: Option<&RowChange>) -> (Option<&CanonRow>, Option<&CanonRow>) {
    match c {
        None => (None, None),
        Some(RowChange::Added(r)) => (None, Some(r)),
        Some(RowChange::Removed(r)) => (Some(r), None),
        Some(RowChange::Changed { before, after }) => (Some(before), Some(after)),
    }
}

/// The change type as a comparable tag (Added/Removed/Changed/None).
fn change_type(c: Option<&RowChange>) -> &'static str {
    match c {
        None => "none",
        Some(RowChange::Added(_)) => "added",
        Some(RowChange::Removed(_)) => "removed",
        Some(RowChange::Changed { .. }) => "changed",
    }
}

fn state_val<'a>(s: Option<&'a CanonRow>, col: &str) -> Option<&'a String> {
    s.and_then(|r| r.get(col))
}

/// Columns whose before- or after-state differs between the sides.
fn differing_columns(
    sb: Option<&CanonRow>,
    sa: Option<&CanonRow>,
    ob: Option<&CanonRow>,
    oa: Option<&CanonRow>,
) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    for r in [sb, sa, ob, oa].into_iter().flatten() {
        for c in r.keys() {
            if !cols.contains(c) {
                cols.push(c.clone());
            }
        }
    }
    cols.sort();
    cols.into_iter()
        .filter(|c| state_val(sb, c) != state_val(ob, c) || state_val(sa, c) != state_val(oa, c))
        .collect()
}

/// Mask volatile columns in a delta so it is comparable across sides. Must be
/// applied to both `delta_sut` and `delta_oracle` before diffing.
pub fn normalize_delta(mut d: Delta, registry: &Registry) -> Delta {
    for (name, table) in d.iter_mut() {
        // Shared across all entries of this table, assigned in natural-key order
        // (BTreeMap iteration) → identical on both sides for equal structure.
        let mut vv_map: HashMap<String, String> = HashMap::new();
        let mut vv_n = 0usize;
        for change in table.values_mut() {
            match change {
                RowChange::Added(row) => mask_row(row, name, registry, &mut vv_map, &mut vv_n),
                RowChange::Removed(row) => mask_row(row, name, registry, &mut vv_map, &mut vv_n),
                RowChange::Changed { before, after } => {
                    mask_row(before, name, registry, &mut vv_map, &mut vv_n);
                    mask_row(after, name, registry, &mut vv_map, &mut vv_n);
                }
            }
        }
    }
    d
}

fn mask_row(
    row: &mut CanonRow,
    table: &str,
    registry: &Registry,
    vv_map: &mut HashMap<String, String>,
    vv_n: &mut usize,
) {
    // Per-row timestamp sentinels preserve equality across columns in this row.
    let mut ts_map: HashMap<String, String> = HashMap::new();
    let mut ts_n = 0usize;

    let cols: Vec<String> = row.keys().cloned().collect();
    for col in cols {
        match registry.class(table, &col).unwrap_or(Class::Stable) {
            Class::TimestampWall => {
                let raw = row.get(&col).cloned().unwrap_or_default();
                let s = ts_map
                    .entry(raw)
                    .or_insert_with(|| {
                        let s = format!("TS{ts_n}");
                        ts_n += 1;
                        s
                    })
                    .clone();
                row.insert(col, s);
            }
            Class::VolatileValue => {
                let raw = row.get(&col).cloned().unwrap_or_default();
                let s = vv_map
                    .entry(raw)
                    .or_insert_with(|| {
                        let s = format!("VV{vv_n}");
                        *vv_n += 1;
                        s
                    })
                    .clone();
                row.insert(col, s);
            }
            _ => {}
        }
    }
}
