//! Known-divergence inventory (the CI gate's divergence-recording
//! prerequisite — the 16.12 `make diff-test`/CI-gate task cannot be green
//! while the accepted divergences exist without this).
//!
//! `divergences.yaml` itemizes every INTENTIONAL divergence the SUT keeps
//! against PHP — with the WHY (the PHP bug or requirement driving it) and a
//! `status` so each can be revisited and removed later.  The scenario runner
//! matches the run's structured divergences against this inventory: an
//! unlisted divergence is a real failure; a listed one is reported as
//! "KNOWN DIVERGENCE (inventory id)" and does not fail the run.
//!
//! Statuses:
//! - `accepted` — the SUT intentionally differs (PHP bug / REQ-driven),
//!   documented with the rationale and a `revisit` hint.
//! - `noise` — a harness artifact (e.g. the second-boundary sentinel-label
//!   differences whose raw values are verified equal), not a behavior gap.

use anyhow::Result;
use serde::Deserialize;

use crate::delta::Divergence;

#[derive(Debug, Clone, Deserialize)]
pub struct DivergenceRecord {
    /// Stable id used in reports and for tracking (remove-by-id later).
    pub id: String,
    /// Why this divergence is intentional / accepted.
    pub why: String,
    /// `accepted` or `noise`.
    pub status: String,
    /// Hint for when this divergence can be revisited/removed.
    #[serde(default)]
    pub revisit: Option<String>,
    /// Scenario names the record applies to; empty = all scenarios.
    #[serde(default)]
    pub scenarios: Vec<String>,
    /// Table of the divergent row(s).
    pub table: String,
    /// Row-key prefix (the canonical natural key), e.g. `"home::admin | "`.
    pub key: String,
    /// Columns whose divergence is covered; empty = any columns.
    #[serde(default)]
    pub columns: Vec<String>,
}

/// The loaded inventory: the records in file order.
pub struct Inventory {
    pub records: Vec<DivergenceRecord>,
}

impl Inventory {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("divergence inventory {path}: {e}"))?;
        let records: Vec<DivergenceRecord> =
            serde_yaml::from_str(&text).map_err(|e| anyhow::anyhow!("inventory {path}: {e}"))?;
        Ok(Self { records })
    }

    /// Split the run's divergences into (known, unlisted).
    ///
    /// A divergence is known when a record matches: same table, the row key
    /// starts with the record's key prefix, the record's columns are empty or
    /// cover the divergence's columns, and the record's scenario list is
    /// empty or contains the scenario name.
    pub fn match_run<'a>(
        &self,
        scenario: &str,
        divs: &'a [Divergence],
    ) -> (Vec<(&'a Divergence, &DivergenceRecord)>, Vec<&'a Divergence>) {
        let mut known = Vec::new();
        let mut unlisted = Vec::new();
        for d in divs {
            match self.records.iter().find(|r| {
                (r.scenarios.is_empty() || r.scenarios.iter().any(|s| s == scenario))
                    && r.table == d.table
                    && d.key.starts_with(&r.key)
                    && (r.columns.is_empty() || d.columns.iter().all(|c| r.columns.contains(c)))
            }) {
                Some(rec) => known.push((d, rec)),
                None => unlisted.push(d),
            }
        }
        (known, unlisted)
    }
}
