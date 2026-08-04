//! File-tree snapshot + delta (Phase 16.8).
//!
//! Compares **bytes**, not just DB rows: a correct DB row with a wrong or
//! missing file on disk is still a divergence (CLAUDE.md hygiene rule 4 —
//! adversarially verify the artifact, not just its visible operation).
//!
//! `data/{user}/files/**` is snapshotted by relative path + size + sha256 via
//! `docker exec <container>`. The volatile subtrees (`files_versions/`,
//! `cache/`, `appdata_{instanceid}/`, trashbin, chunked `uploads/`) live
//! OUTSIDE `files/` by construction, so rooting the snapshot there excludes
//! them; in-flight `*.part` partials are excluded defensively within.
//!
//! Timestamps are deliberately not snapshotted: the DB delta already covers
//! mtime semantics with equality-preserving masking; the file tree compares
//! content identity only.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use crate::config::Instance;

/// One file in the tree: size + content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub sha256: String,
}

/// Relative path (e.g. `hello.txt`, `Media/file`) -> entry.
pub type FileTree = BTreeMap<String, FileEntry>;

/// Snapshot the user's files tree inside the instance's container.
pub async fn snapshot_tree(inst: &Instance, data_dir: &str, user: &str) -> Result<FileTree> {
    let root = format!("{data_dir}/{user}/files");
    // Two passes in one exec: sizes (tab-separated), a marker, then content
    // hashes. Paths containing tabs/newlines would break this parsing — the
    // scenario fixtures are tame by design (documented limitation).
    let script = format!(
        "cd '{root}' 2>/dev/null || exit 0; \
         find . -type f ! -name '*.part' -printf '%s\\t%p\\n'; \
         echo '---SHA256---'; \
         find . -type f ! -name '*.part' -print0 | xargs -0 -r sha256sum"
    );
    let out = tokio::process::Command::new("docker")
        .args(["exec", &inst.container, "sh", "-c", &script])
        .output()
        .await
        .with_context(|| format!("docker exec {}", inst.container))?;
    if !out.status.success() {
        bail!(
            "docker exec {} failed ({}): {}",
            inst.container,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8(out.stdout).context("non-UTF8 docker exec output")?;
    parse_snapshot(&stdout)
        .with_context(|| format!("parsing file-tree snapshot of {}", inst.container))
}

/// Parse the two-section `docker exec` output into a [`FileTree`].
fn parse_snapshot(stdout: &str) -> Result<FileTree> {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    let mut tree = FileTree::new();
    let mut in_hashes = false;
    for line in stdout.lines() {
        if line == "---SHA256---" {
            in_hashes = true;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if !in_hashes {
            let (size, path) = line.split_once('\t').context("size line without tab")?;
            let size: u64 = size.parse().with_context(|| format!("bad size {size:?}"))?;
            sizes.insert(norm(path), size);
        } else {
            // sha256sum text-mode output: `{64-hex}  {path}` (two spaces).
            let hash = line.get(..64).context("short sha256sum line")?;
            let path = line
                .get(66..)
                .with_context(|| format!("bad sha256sum line: {line:.80}"))?;
            let p = norm(path);
            let size = sizes
                .remove(&p)
                .with_context(|| format!("hash without size entry for {p:?}"))?;
            tree.insert(p, FileEntry { size, sha256: hash.to_string() });
        }
    }
    if !sizes.is_empty() {
        bail!("{} file(s) had a size entry but no hash", sizes.len());
    }
    Ok(tree)
}

/// Strip the leading `./` emitted by `find`/`sha256sum`.
fn norm(p: &str) -> String {
    p.trim_start_matches("./").to_string()
}

/// Change of one file between two trees.
#[derive(Debug, Clone, PartialEq)]
pub enum FileChange {
    Added(FileEntry),
    Removed(FileEntry),
    Changed { before: FileEntry, after: FileEntry },
}

/// Relative path -> change, between the before and after trees of ONE side.
pub type FileDelta = BTreeMap<String, FileChange>;

pub fn delta(before: &FileTree, after: &FileTree) -> FileDelta {
    let mut d = FileDelta::new();
    for (p, a) in after {
        match before.get(p) {
            None => {
                d.insert(p.clone(), FileChange::Added(a.clone()));
            }
            Some(b) if b != a => {
                d.insert(
                    p.clone(),
                    FileChange::Changed {
                        before: b.clone(),
                        after: a.clone(),
                    },
                );
            }
            Some(_) => {}
        }
    }
    for (p, b) in before {
        if !after.contains_key(p) {
            d.insert(p.clone(), FileChange::Removed(b.clone()));
        }
    }
    d
}

/// Render a file delta deterministically (mirrors `report::render` style).
pub fn render(d: &FileDelta) -> String {
    let mut out = String::new();
    if d.is_empty() {
        out.push_str("(no changes)\n");
        return out;
    }
    out.push_str("== files\n");
    for (path, change) in d {
        match change {
            FileChange::Added(e) => {
                out.push_str(&format!("  + {path} ({} bytes, sha256:{})\n", e.size, e.sha256));
            }
            FileChange::Removed(e) => {
                out.push_str(&format!("  - {path} ({} bytes, sha256:{})\n", e.size, e.sha256));
            }
            FileChange::Changed { before, after } => {
                out.push_str(&format!(
                    "  ~ {path} size: {} -> {}; sha256:{} -> {}\n",
                    before.size, after.size, before.sha256, after.sha256
                ));
            }
        }
    }
    out
}

/// Diff two file deltas. Returns `(identical, unified_diff)`.
pub fn diff(sut: &FileDelta, oracle: &FileDelta) -> (bool, String) {
    use similar::{ChangeTag, TextDiff};
    let a = render(sut);
    let b = render(oracle);
    if a == b {
        return (true, String::new());
    }
    let diff = TextDiff::from_lines(&a, &b);
    let mut out = String::new();
    out.push_str("--- SUT file delta\n+++ Oracle file delta\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_snapshot_basic() {
        let out = "26\t./hello.txt\n5\t./Media/a.txt\n---SHA256---\n\
                   0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ./hello.txt\n\
                   fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  ./Media/a.txt\n";
        let tree = parse_snapshot(out).unwrap();
        assert_eq!(tree.len(), 2);
        let hello = &tree["hello.txt"];
        assert_eq!(hello.size, 26);
        assert!(hello.sha256.starts_with("0123456789abcdef"));
        assert_eq!(tree["Media/a.txt"].size, 5);
    }

    #[test]
    fn parse_empty_tree() {
        let tree = parse_snapshot("---SHA256---\n").unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn delta_added_removed_changed() {
        let e1 = FileEntry { size: 1, sha256: "a".repeat(64) };
        let e2 = FileEntry { size: 2, sha256: "b".repeat(64) };
        let e3 = FileEntry { size: 1, sha256: "c".repeat(64) };
        let before: FileTree =
            [("gone".into(), e1.clone()), ("same".into(), e2.clone()), ("mut".into(), e1.clone())]
                .into_iter()
                .collect();
        let after: FileTree =
            [("new".into(), e3.clone()), ("same".into(), e2.clone()), ("mut".into(), e3.clone())]
                .into_iter()
                .collect();
        let d = delta(&before, &after);
        assert_eq!(d.len(), 3);
        assert!(matches!(d["new"], FileChange::Added(_)));
        assert!(matches!(d["gone"], FileChange::Removed(_)));
        assert!(matches!(d["mut"], FileChange::Changed { .. }));
        assert!(!d.contains_key("same"));
    }

    #[test]
    fn identical_deltas_diff_empty() {
        let e = FileEntry { size: 26, sha256: "x".repeat(64) };
        let mut d = FileDelta::new();
        d.insert("hello.txt".into(), FileChange::Added(e.clone()));
        let (identical, text) = diff(&d, &d);
        assert!(identical);
        assert!(text.is_empty());

        let mut d2 = FileDelta::new();
        d2.insert(
            "hello.txt".into(),
            FileChange::Added(FileEntry { size: 27, sha256: "y".repeat(64) }),
        );
        let (identical2, text2) = diff(&d, &d2);
        assert!(!identical2);
        assert!(text2.contains("hello.txt"));
    }
}
