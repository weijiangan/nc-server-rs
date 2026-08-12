//! Unified-diff rendering of canonical deltas (Phase 16.4).
//!
//! Renders each side's normalized delta to a deterministic text form and diffs
//! them with `similar`. An empty diff means the two sides behaved identically;
//! otherwise the diff is an actionable per-table/per-row report.

use similar::{ChangeTag, TextDiff};

use crate::delta::{Delta, RowChange};

/// Render a normalized delta to a deterministic, human-readable string.
pub fn render(d: &Delta) -> String {
    let mut out = String::new();
    if d.is_empty() {
        out.push_str("(no changes)\n");
        return out;
    }
    for (table, changes) in d {
        out.push_str(&format!("== {table}\n"));
        for (key, change) in changes {
            let key = sanitize(key);
            match change {
                RowChange::Added(row) => {
                    out.push_str(&format!("  + {key} {}\n", fmt_row(row)));
                }
                RowChange::Removed(row) => {
                    out.push_str(&format!("  - {key} {}\n", fmt_row(row)));
                }
                RowChange::Changed { before, after } => {
                    let mut diffs = Vec::new();
                    let mut cols: Vec<&String> = before.keys().collect();
                    for c in after.keys() {
                        if !before.contains_key(c) {
                            cols.push(c);
                        }
                    }
                    cols.sort();
                    for c in cols {
                        let b = before
                            .get(c)
                            .map(|s| sanitize(s))
                            .unwrap_or_else(|| "∅".into());
                        let a = after
                            .get(c)
                            .map(|s| sanitize(s))
                            .unwrap_or_else(|| "∅".into());
                        if b != a {
                            diffs.push(format!("{c}: {b} -> {a}"));
                        }
                    }
                    out.push_str(&format!("  ~ {key} {}\n", diffs.join("; ")));
                }
            }
        }
    }
    out
}

fn fmt_row(row: &std::collections::BTreeMap<String, String>) -> String {
    let parts: Vec<String> = row
        .iter()
        .map(|(k, v)| format!("{k}={}", sanitize(v)))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// Replace the control separators used in natural keys / nulls for display.
fn sanitize(s: &str) -> String {
    s.replace('\u{1}', " | ").replace('\u{0}', "∅")
}

/// Diff two normalized deltas. Returns `(identical, unified_diff)`.
pub fn diff(sut: &Delta, oracle: &Delta) -> (bool, String) {
    let a = render(sut);
    let b = render(oracle);
    if a == b {
        return (true, String::new());
    }
    let diff = TextDiff::from_lines(&a, &b);
    let mut out = String::new();
    out.push_str("--- SUT delta\n+++ Oracle delta\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(&format!("{sign}{change}"));
    }
    (false, out)
}
