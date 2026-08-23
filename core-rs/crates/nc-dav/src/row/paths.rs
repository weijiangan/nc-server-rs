use md5::{Digest, Md5};


// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Convert a WebDAV path (e.g. `/Photos/img.jpg` or `/`) to the path stored in
/// `oc_filecache` (e.g. `files/Photos/img.jpg` or `files`).
///
/// Nextcloud's home storage keeps all user files under a `files/` prefix.
pub fn dav_to_fc_path(dav_path: &str) -> String {
    let trimmed = dav_path.trim_matches('/');
    if trimmed.is_empty() {
        "files".to_string()
    } else {
        format!("files/{trimmed}")
    }
}


/// Compute the MD5 path hash used by `oc_filecache.path_hash`.
pub fn path_hash(path: &str) -> String {
    format!("{:x}", Md5::digest(path.as_bytes()))
}


/// Derive the disk path for a filecache entry on a local home storage.
///
/// Layout: `{data_directory}/{uid}/{fc_path}`
/// where `fc_path` already has the `files/` prefix (e.g. `files/Photos/img.jpg`).
pub fn disk_path(data_dir: &std::path::Path, uid: &str, fc_path: &str) -> std::path::PathBuf {
    data_dir.join(uid).join(fc_path)
}


// ─── Filecache subtree mutations (phase 24) ──────────────────────────────────

/// The 1-based `SUBSTRING` offset that strips `prefix` from a path.
///
/// Postgres' `SUBSTRING(x FROM n)` counts **characters**, so the offset is
/// the character length — PHP's `mb_strlen($sourcePath) + 1`
/// (`Cache.php:751`), not the byte length that Rust slicing uses.
pub(crate) fn subtree_suffix_offset(prefix: &str) -> i32 {
    prefix.chars().count() as i32 + 1
}
