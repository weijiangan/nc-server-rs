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
