//! Nextcloud-specific DAV properties for the `{oc:}` and `{nc:}` namespaces.
//!
//! CONFORMANCE FIXES (vs original Phase 4 implementation):
//! - Removed bogus `oc:has-preview` (only `nc:has-preview` is valid, REQ §6.5)
//! - Fixed `{oc:}checksums` to wrap each entry in inner `<oc:checksum>` element
//! - Fixed `CK` double-emit when both UPDATE (bit 2) and CREATE (bit 4) set on dir
//! - Added `{oc:}data-fingerprint`, `{nc:}mount-type`, `{nc:}is-mount-root`,
//!   `{nc:}is-federated`, `{nc:}contained-folder-count`, `{nc:}contained-file-count`
//! - `build_props` now takes `data_fingerprint`, `child_dir_count`, `child_file_count`
//!
//! ## References
//! - REQ §4.7 — Standard DAV properties
//! - REQ §4.8 — `{oc:}` namespace properties
//! - REQ §4.9 — `{nc:}` namespace properties

use dav_server::fs::DavProp;

use crate::metadata::NcMetaData;

// ─── Namespaces ───────────────────────────────────────────────────────────────

pub const OC_NS: &str  = "http://owncloud.org/ns";
pub const NC_NS: &str  = "http://nextcloud.org/ns";
/// OCS (Open Collaboration Services) namespace — used for `{ocs:}share-permissions`.
pub const OCS_NS: &str = "http://open-collaboration-services.org/ns";

// ─── Public API ───────────────────────────────────────────────────────────────

/// Build the full list of `{oc:}` and `{nc:}` properties for `path`.
///
/// When `do_content = false` only the property *names* (no values) are
/// returned — this is used for `allprop` and `propname` requests.
///
/// - `data_fingerprint`: value of `core/data-fingerprint` from `oc_appconfig`.
/// - `owner_display_name`: resolved from `oc_users.displayname`; falls back to
///   `uid` when no display name is set.  Used for `{oc:}owner-display-name`.
/// - `child_dir_count` / `child_file_count`: count of direct child directories
///   and files; pass 0 for non-directories.
/// - `sync_token`: RFC 6578 sync token string for collection nodes, e.g.
///   `"http://sabre.io/ns/sync/1705322096"`.  Pass `None` for files or when
///   the token has not been computed.  Emitted as `{DAV:}sync-token` only
///   when `Some` (PHASE-4.11).
/// - `is_mounted`: `true` when the file lives on a non-home mount (i.e.
///   `oc_storages.id` does not start with `"home::"`).  Adds the `M` flag to
///   `{oc:}permissions` (PHASE-7.6).
/// - `share_permissions`: MAX permissions bitmask from `oc_share` for this
///   file and owner; pass `31` when the file has no share rows (PHASE-7.6).
/// - `download_url`: direct-download URL for home-storage files, built from
///   `overwrite.cli.url` + `/remote.php/webdav/{path}`; pass `""` for
///   non-home storage (PHASE-7.6).
/// - `note`: most-recent non-empty `oc_share.note` for this file (PHASE-7.6).
pub fn build_props(
    meta: &NcMetaData,
    instance_id: &str,
    uid: &str,
    owner_display_name: &str,
    do_content: bool,
    data_fingerprint: &str,
    child_dir_count: i64,
    child_file_count: i64,
    sync_token: Option<&str>,
    is_mounted: bool,
    share_permissions: i32,
    download_url: &str,
    note: &str,
) -> Vec<DavProp> {
    if !do_content {
        return prop_names();
    }

    let perms_str = encode_permissions(meta.permissions, meta.is_dir_flag, is_mounted);

    // {oc:}id = fileid zero-padded to 8 chars + instance_id (REQ §4.8)
    let oc_id = format!("{:08}{}", meta.fileid, instance_id);

    // {oc:}checksums: each entry wrapped in <oc:checksum> inner element (REQ §6.5)
    let checksums_val = match &meta.checksum {
        Some(cs) if !cs.is_empty() => {
            format!("<oc:checksum xmlns:oc=\"{OC_NS}\">{cs}</oc:checksum>")
        }
        _ => String::new(),
    };

    // {nc:}is-mount-root: "true" when the node IS the storage mount root.
    // In oc_filecache the home storage root has path "files" (or ""); its
    // DAV "internal path" (relative to the mount) is then empty — Nextcloud
    // marks that node as the mount root (REQ §6.5, PHASE-4.9).
    let is_mount_root = matches!(
        meta.path.as_deref(),
        Some("") | Some("files")
    );
    let is_mount_root_str = if is_mount_root { "true" } else { "false" };

    let mut props = vec![
        // ── oc: ──────────────────────────────────────────────────────────
        make_prop("id",                   "oc", OC_NS, &oc_id),
        make_prop("fileid",               "oc", OC_NS, &meta.fileid.to_string()),
        make_prop("permissions",          "oc", OC_NS, &perms_str),
        make_prop("size",                 "oc", OC_NS, &meta.size.to_string()),
        make_prop("owner-id",             "oc",  OC_NS, uid),
        // {oc:}owner-display-name — resolved from oc_users.displayname (REQ §6.5 / §4.8)
        make_prop("owner-display-name",   "oc",  OC_NS, owner_display_name),
        make_prop("etag",                 "oc",  OC_NS, meta.etag.as_deref().unwrap_or("")),
        make_prop("checksums",            "oc",  OC_NS, &checksums_val),
        make_prop("data-fingerprint",     "oc",  OC_NS, data_fingerprint),
        make_prop("downloadURL",          "oc",  OC_NS, download_url),
        make_prop("tags",                 "oc",  OC_NS, ""),
        make_prop("favorite",             "oc",  OC_NS, "0"),
        // {ocs:}share-permissions — per-share MAX permissions from oc_share;
        // defaults to 31 (all permissions) for the owner's own unshared file
        // (REQ §6.5 / §4.8, PHASE-7.6).
        make_prop("share-permissions",    "ocs", OCS_NS, &share_permissions.to_string()),

        // ── nc: ──────────────────────────────────────────────────────────
        // NOTE: nc:has-preview only — oc:has-preview does NOT exist (REQ §6.5)
        make_prop("has-preview",              "nc", NC_NS, "false"),
        make_prop("creation_time",            "nc", NC_NS, &meta.creation_time.to_string()),
        make_prop("upload_time",              "nc", NC_NS, &meta.upload_time.to_string()),
        make_prop("mount-type",               "nc", NC_NS, "local"),
        make_prop("is-mount-root",            "nc", NC_NS, is_mount_root_str),
        make_prop("is-federated",             "nc", NC_NS, "false"),
        // {nc:}hide-download — relevant for public shares; always "false" for
        // home-storage nodes (REQ §6.5, PHASE-4.9)
        make_prop("hide-download",            "nc", NC_NS, "false"),
        make_prop("contained-folder-count",   "nc", NC_NS, &child_dir_count.to_string()),
        make_prop("contained-file-count",     "nc", NC_NS, &child_file_count.to_string()),
        make_prop("remind-me-at",             "nc", NC_NS, ""),
        make_prop("note",                     "nc", NC_NS, note),
        make_prop("hidden",                   "nc", NC_NS, "false"),
        make_prop("share-attributes",         "nc", NC_NS, ""),
        make_prop("acl-can-read",             "nc", NC_NS, "true"),
        make_prop("acl-can-write",            "nc", NC_NS, "true"),
        make_prop("acl-can-delete",           "nc", NC_NS, "true"),
        make_prop("acl-can-manage",           "nc", NC_NS, "true"),

        // ── DAV quota (unlimited) ─────────────────────────────────────────
        //
        // dav-server emits `{DAV:}quota-available-bytes` only when
        // `DavFileSystem::get_quota()` returns `Some(total)`.  We return
        // `None` (unlimited quota) so dav-server suppresses that prop and we
        // inject the Nextcloud sentinel value `-3` (SPACE_UNLIMITED, REQ §6.5)
        // here without producing a duplicate.
        //
        // `{DAV:}quota-used-bytes` is handled entirely by dav-server using
        // the `used` first-element from `get_quota()` and does NOT need to
        // appear here.
        make_prop("quota-available-bytes", "D", "DAV:", "-3"),
    ];

    // Conditionally include metadata_etag if present.
    if let Some(ref etag) = meta.metadata_etag {
        props.push(make_prop("metadata_etag", "nc", NC_NS, etag));
    }

    // {DAV:}sync-token — RFC 6578 delta sync; only on collections (PHASE-4.11).
    // Emitted only when the caller has computed and provided the token value.
    if let Some(token) = sync_token {
        if meta.is_dir_flag {
            // dav-server parses {DAV:} props with prefix "D"; using "d" produces
            // duplicate namespace declarations so we use "D" for consistency.
            props.push(make_prop("sync-token", "D", "DAV:", token));
        }
    }

    props
}

// ─── Permission encoding ──────────────────────────────────────────────────────

/// Encode an `oc_filecache.permissions` bitmask to the Nextcloud permission
/// string used in `{oc:}permissions`.
///
/// Bitmask constants (from `\OCP\Constants`):
/// - `1`  = READ
/// - `2`  = UPDATE / WRITE
/// - `4`  = CREATE
/// - `8`  = DELETE
/// - `16` = SHARE
///
/// Encoded characters:
/// - `G`  = READ
/// - `NV` = UPDATE (rename + write)
/// - `CK` = CREATE (upload into / create dir inside)
///   On directories: also appended after `NV` **only if bit 4 is absent**,
///   to indicate files can be created inside. Avoids double-emitting `CK`.
/// - `D`  = DELETE
/// - `S`  = SHARE
/// - `M`  = MOUNTED — set when `is_mounted` is `true` (PHASE-7.6)
pub fn encode_permissions(perms: i32, is_dir: bool, is_mounted: bool) -> String {
    let mut s = String::new();
    if perms & 16 != 0 { s.push('S'); }
    if perms & 4  != 0 { s.push_str("CK"); }
    if perms & 8  != 0 { s.push('D'); }
    if perms & 2  != 0 {
        s.push_str("NV");
        // For dirs: add CK to signal "create inside" only when bit 4 hasn't already
        // contributed CK above (to avoid doubling it when both UPDATE+CREATE set).
        if is_dir && perms & 4 == 0 {
            s.push_str("CK");
        }
    }
    if perms & 1  != 0 { s.push('G'); }
    if is_mounted { s.push('M'); }
    s
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Build a `DavProp` with XML content.
///
/// Produces: `<pfx:name xmlns:pfx="ns">value</pfx:name>`
pub fn make_prop(name: &str, prefix: &str, ns: &str, value: &str) -> DavProp {
    DavProp::new(
        name.to_string(),
        prefix.to_string(),
        ns.to_string(),
        value.to_string(),
    )
}

/// Return the list of all Nextcloud custom property names (without values).
/// Used for `allprop` / `propname` responses.
fn prop_names() -> Vec<DavProp> {
    fn name_only(name: &str, prefix: &str, ns: &str) -> DavProp {
        DavProp {
            name:      name.to_string(),
            prefix:    Some(prefix.to_string()),
            namespace: Some(ns.to_string()),
            xml:       None,
        }
    }

    vec![
        name_only("id",                       "oc", OC_NS),
        name_only("fileid",                   "oc", OC_NS),
        name_only("permissions",              "oc", OC_NS),
        name_only("size",                     "oc", OC_NS),
        name_only("owner-id",                 "oc",  OC_NS),
        name_only("owner-display-name",       "oc",  OC_NS),
        name_only("etag",                     "oc",  OC_NS),
        name_only("checksums",                "oc",  OC_NS),
        name_only("data-fingerprint",         "oc",  OC_NS),
        name_only("downloadURL",              "oc",  OC_NS),
        name_only("tags",                     "oc",  OC_NS),
        name_only("favorite",                 "oc",  OC_NS),
        name_only("share-permissions",        "ocs", OCS_NS),
        name_only("has-preview",              "nc", NC_NS),
        name_only("creation_time",            "nc", NC_NS),
        name_only("upload_time",              "nc", NC_NS),
        name_only("metadata_etag",            "nc", NC_NS),
        name_only("mount-type",               "nc", NC_NS),
        name_only("is-mount-root",            "nc", NC_NS),
        name_only("is-federated",             "nc", NC_NS),
        name_only("hide-download",            "nc", NC_NS),
        name_only("contained-folder-count",   "nc", NC_NS),
        name_only("contained-file-count",     "nc", NC_NS),
        name_only("remind-me-at",             "nc", NC_NS),
        name_only("note",                     "nc", NC_NS),
        name_only("hidden",                   "nc", NC_NS),
        name_only("share-attributes",         "nc", NC_NS),
        name_only("acl-can-read",             "nc", NC_NS),
        name_only("acl-can-write",            "nc", NC_NS),
        name_only("acl-can-delete",           "nc", NC_NS),
        name_only("acl-can-manage",           "nc", NC_NS),
        // {DAV:}sync-token: RFC 6578 delta sync property on collections
        // (PHASE-4.11).  Listed here so allprop/propname responses include it.
        name_only("sync-token",               "D",  "DAV:"),
        // Note: {DAV:}quota-available-bytes is listed by dav-server's own
        // allprop/propname set so we do NOT duplicate it here.
    ]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::NcMetaData;

    fn test_meta(checksum: Option<&str>) -> NcMetaData {
        NcMetaData {
            fileid: 42, size: 100, mtime: 1700000000, is_dir_flag: false,
            mime_type: "text/plain".into(),
            etag: Some("abc".into()), permissions: 27,
            creation_time: 1700000000, upload_time: 1700000000,
            checksum: checksum.map(String::from),
            display_name: "test.txt".into(), metadata_etag: None,
            storage: 1, path: Some("files/test.txt".into()), parent: 2,
        }
    }

    /// Call `build_props` with sensible defaults.  Extra params added in §4.8,
    /// §4.11, and §7.6: `owner_display_name` as `"Alice Test"`; `sync_token = None`;
    /// `is_mounted = false`; `share_permissions = 31`; `download_url = ""`; `note = ""`.
    fn build(meta: &NcMetaData) -> Vec<DavProp> {
        build_props(meta, "inst", "alice", "Alice Test", true, "", 0, 0, None, false, 31, "", "")
    }

    // ── encode_permissions ────────────────────────────────────────────────────

    #[test]
    fn permissions_file_all() {
        let s = encode_permissions(31, false, false);
        assert!(s.contains('S'),  "missing S:  {s}");
        assert!(s.contains("CK"),"missing CK: {s}");
        assert!(s.contains('D'), "missing D:  {s}");
        assert!(s.contains("NV"),"missing NV: {s}");
        assert!(s.contains('G'), "missing G:  {s}");
    }

    // ── owner-display-name (§4.8) ─────────────────────────────────────────────

    #[test]
    fn owner_display_name_uses_provided_value() {
        let props = build(&test_meta(None));
        let p = props.iter().find(|p| p.name == "owner-display-name").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("Alice Test"), "should contain display name: {xml}");
    }

    #[test]
    fn owner_display_name_not_same_as_uid_when_different() {
        let props = build_props(&test_meta(None), "inst", "alice", "Alice Test", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "owner-display-name").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        // Display name must be "Alice Test", not the raw uid "alice"
        assert!(!xml.contains(">alice<"), "must not contain raw uid: {xml}");
    }

    // ── downloadURL (§4.8 / PHASE-7.6) ───────────────────────────────────────────

    #[test]
    fn download_url_empty_when_not_provided() {
        let props = build(&test_meta(None));
        let p = props
            .iter()
            .find(|p| p.name == "downloadURL" && p.namespace.as_deref() == Some(OC_NS))
            .expect("{oc:}downloadURL must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(!xml.is_empty(), "xml element should be present even if value is empty: {xml}");
    }

    #[test]
    fn download_url_set_when_provided() {
        let url = "https://nc.example.com/remote.php/webdav/Photos/img.jpg";
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, url, "");
        let p = props
            .iter()
            .find(|p| p.name == "downloadURL" && p.namespace.as_deref() == Some(OC_NS))
            .expect("{oc:}downloadURL must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains(url), "downloadURL must contain the provided URL: {xml}");
    }

    // ── is_mounted / M flag (PHASE-7.6) ──────────────────────────────────────

    #[test]
    fn permissions_m_flag_when_mounted() {
        // Any storage with is_mounted=true should produce M in {oc:}permissions
        let s = encode_permissions(1, false, true);
        assert!(s.contains('M'), "M flag must be present when is_mounted=true: {s}");
    }

    #[test]
    fn permissions_no_m_flag_when_not_mounted() {
        let s = encode_permissions(31, false, false);
        assert!(!s.contains('M'), "M flag must NOT be present for home storage: {s}");
    }

    #[test]
    fn permissions_m_flag_in_full_perms_string() {
        let s = encode_permissions(31, false, true);
        assert!(s.contains('M'), "M must appear in full permissions string when mounted: {s}");
        // All other flags must still be present
        assert!(s.contains('G'), "G must still be present: {s}");
        assert!(s.contains("NV"), "NV must still be present: {s}");
        assert!(s.contains("CK"), "CK must still be present: {s}");
        assert!(s.contains('D'), "D must still be present: {s}");
        assert!(s.contains('S'), "S must still be present: {s}");
    }

    // ── share-permissions real value (PHASE-7.6) ──────────────────────────────

    #[test]
    fn share_permissions_reflects_passed_value() {
        // A file shared read-only has permissions=1 in oc_share
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 1, "", "");
        let p = props
            .iter()
            .find(|p| p.name == "share-permissions" && p.namespace.as_deref() == Some(OCS_NS))
            .expect("{ocs:}share-permissions must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains(">1<"), "share-permissions must reflect passed value 1: {xml}");
    }

    // ── note (PHASE-7.6) ──────────────────────────────────────────────────────

    #[test]
    fn note_empty_by_default() {
        let props = build(&test_meta(None));
        let p = props.iter().find(|p| p.name == "note").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        // Empty note: just the wrapping element
        assert!(!xml.contains("hello"), "should not contain note text when empty: {xml}");
    }

    #[test]
    fn note_propagated_when_provided() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "hello share note");
        let p = props.iter().find(|p| p.name == "note").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("hello share note"), "note must contain provided text: {xml}");
    }

    // ── share-permissions (§4.8) ──────────────────────────────────────────────

    #[test]
    fn share_permissions_present_in_ocs_namespace() {
        let props = build(&test_meta(None));
        let p = props
            .iter()
            .find(|p| p.name == "share-permissions" && p.namespace.as_deref() == Some(OCS_NS))
            .expect("{ocs:}share-permissions must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("31"), "default share-permissions should be 31: {xml}");
    }

    #[test]
    fn share_permissions_not_in_oc_namespace() {
        let props = build(&test_meta(None));
        for p in &props {
            if p.name == "share-permissions" {
                let ns = p.namespace.as_deref().unwrap_or("");
                assert_ne!(ns, OC_NS, "share-permissions must NOT be in oc namespace");
            }
        }
    }

    #[test]
    fn permissions_dir_no_create() {
        assert_eq!(encode_permissions(1, true, false), "G");
    }

    #[test]
    fn permissions_dir_with_update_gets_ck() {
        let s = encode_permissions(2, true, false);
        assert!(s.contains("CK"), "dir UPDATE should include CK: {s}");
    }

    #[test]
    fn permissions_dir_update_and_create_no_duplicate_ck() {
        // bits 4|2 on dir: CK from bit 4 only — must not appear twice
        let s = encode_permissions(6, true, false);
        assert_eq!(s.matches("CK").count(), 1, "CK double-emitted: {s}");
    }

    #[test]
    fn permissions_file_update_and_create_no_extra_ck() {
        // bits 4|2 on file: CK from bit 4 only, not from UPDATE
        let s = encode_permissions(6, false, false);
        assert_eq!(s.matches("CK").count(), 1, "file CK count wrong: {s}");
    }

    // ── checksums ─────────────────────────────────────────────────────────────

    #[test]
    fn checksums_wraps_in_checksum_element() {
        let props = build_props(&test_meta(Some("SHA1:abc123")), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "checksums").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("oc:checksum"), "missing inner element: {xml}");
        assert!(xml.contains("SHA1:abc123"), "missing value: {xml}");
    }

    #[test]
    fn checksums_empty_when_no_checksum() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "checksums").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        // Should just be <oc:checksums …></oc:checksums> with no inner element
        assert!(!xml.contains("oc:checksum>S"), "should not have checksum when none stored: {xml}");
    }

    // ── namespace correctness ─────────────────────────────────────────────────

    #[test]
    fn no_oc_has_preview_only_nc_has_preview() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let oc_pv = props.iter().find(|p| p.name == "has-preview" && p.namespace.as_deref() == Some(OC_NS));
        let nc_pv = props.iter().find(|p| p.name == "has-preview" && p.namespace.as_deref() == Some(NC_NS));
        assert!(oc_pv.is_none(), "oc:has-preview must not be emitted");
        assert!(nc_pv.is_some(), "nc:has-preview must be emitted");
    }

    // ── new required props ────────────────────────────────────────────────────

    // ── is-mount-root (§4.9) ──────────────────────────────────────────────

    fn test_meta_with_path(path: Option<&str>) -> NcMetaData {
        NcMetaData {
            fileid: 1, size: 0, mtime: 0, is_dir_flag: true,
            mime_type: "httpd/unix-directory".into(),
            etag: None, permissions: 31,
            creation_time: 0, upload_time: 0, checksum: None,
            display_name: "".into(), metadata_etag: None,
            storage: 1, path: path.map(String::from), parent: 0,
        }
    }

    #[test]
    fn is_mount_root_true_for_files_path() {
        let meta = test_meta_with_path(Some("files"));
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("true"), "is-mount-root must be true for path=files: {xml}");
    }

    #[test]
    fn is_mount_root_true_for_empty_path() {
        let meta = test_meta_with_path(Some(""));
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("true"), "is-mount-root must be true for path=\"\": {xml}");
    }

    #[test]
    fn is_mount_root_false_for_subdir() {
        let meta = test_meta_with_path(Some("files/Photos"));
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("false"), "is-mount-root must be false for subdir: {xml}");
    }

    // ── hide-download (§4.9) ──────────────────────────────────────────────────

    #[test]
    fn hide_download_present_and_false() {
        let props = build(&test_meta(None));
        let p = props
            .iter()
            .find(|p| p.name == "hide-download" && p.namespace.as_deref() == Some(NC_NS))
            .expect("{nc:}hide-download must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("false"), "{{nc:}}hide-download must be false for home nodes: {xml}");
    }

    #[test]
    fn hide_download_in_prop_names() {
        let meta = test_meta(None);
        let names = build_props(&meta, "inst", "u", "U", false, "", 0, 0, None, false, 31, "", "");
        let found = names.iter().any(|p| p.name == "hide-download" && p.namespace.as_deref() == Some(NC_NS));
        assert!(found, "{{nc:}}hide-download must appear in propnames response");
    }

    // ── sync-token (§4.11) ────────────────────────────────────────────────────

    #[test]
    fn sync_token_emitted_for_dir_when_provided() {
        let mut meta = test_meta_with_path(Some("files"));
        // make it a directory
        meta.mime_type = "httpd/unix-directory".into();
        meta.is_dir_flag = true;
        let token_val = "http://sabre.io/ns/sync/1705322096";
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0, Some(token_val), false, 31, "", "");
        let p = props
            .iter()
            .find(|p| p.name == "sync-token" && p.namespace.as_deref() == Some("DAV:"))
            .expect("{DAV:}sync-token must be present on a directory when provided");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains(token_val), "sync-token value wrong: {xml}");
    }

    #[test]
    fn sync_token_not_emitted_for_file() {
        let meta = test_meta(None); // is_dir_flag = false
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0,
            Some("http://sabre.io/ns/sync/0"), false, 31, "", "");
        let found = props.iter().any(|p| p.name == "sync-token");
        assert!(!found, "sync-token must NOT appear on a file node");
    }

    #[test]
    fn sync_token_not_emitted_when_none() {
        let mut meta = test_meta(None);
        meta.is_dir_flag = true;
        let props = build_props(&meta, "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let found = props.iter().any(|p| p.name == "sync-token");
        assert!(!found, "sync-token must not appear when None is passed");
    }

    #[test]
    fn sync_token_in_prop_names() {
        let meta = test_meta(None);
        let names = build_props(&meta, "inst", "u", "U", false, "", 0, 0, None, false, 31, "", "");
        let found = names.iter().any(|p| p.name == "sync-token" && p.namespace.as_deref() == Some("DAV:"));
        assert!(found, "{{DAV:}}sync-token must appear in propnames response");
    }

    #[test]
    fn data_fingerprint_present() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "fp123", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "data-fingerprint").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("fp123"), "{xml}");
    }

    #[test]
    fn mount_type_is_local() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props.iter().find(|p| p.name == "mount-type").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("local"), "{xml}");
    }

    #[test]
    fn child_counts_present() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 3, 7, None, false, 31, "", "");
        let dirs = props.iter().find(|p| p.name == "contained-folder-count").unwrap();
        let files = props.iter().find(|p| p.name == "contained-file-count").unwrap();
        let dirs_xml  = std::str::from_utf8(dirs.xml.as_ref().unwrap()).unwrap();
        let files_xml = std::str::from_utf8(files.xml.as_ref().unwrap()).unwrap();
        assert!(dirs_xml.contains('3'),  "{dirs_xml}");
        assert!(files_xml.contains('7'), "{files_xml}");
    }

    // ── DAV quota properties (REQ §6.5 / PHASE-4.7) ──────────────────────────────

    /// `{DAV:}quota-available-bytes` must be present with value "-3" (SPACE_UNLIMITED).
    /// dav-server suppresses its own emit when `get_quota()` returns `None` for total,
    /// so we inject it here without producing a duplicate.
    #[test]
    fn quota_available_bytes_is_minus_three() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        let p = props
            .iter()
            .find(|p| p.name == "quota-available-bytes" && p.namespace.as_deref() == Some("DAV:"))
            .expect("{DAV:}quota-available-bytes must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("-3"), "quota-available-bytes must be -3 (SPACE_UNLIMITED): {xml}");
    }

    /// `{DAV:}quota-available-bytes` must NOT appear under the OC or NC namespace.
    #[test]
    fn quota_available_bytes_not_in_oc_or_nc_namespace() {
        let props = build_props(&test_meta(None), "inst", "u", "U", true, "", 0, 0, None, false, 31, "", "");
        for p in &props {
            if p.name == "quota-available-bytes" {
                let ns = p.namespace.as_deref().unwrap_or("");
                assert_eq!(ns, "DAV:", "quota-available-bytes must be in DAV: namespace, got {ns}");
            }
        }
    }

    // ── make_prop ─────────────────────────────────────────────────────────────

    #[test]
    fn make_prop_produces_xml() {
        let p = make_prop("fileid", "oc", OC_NS, "42");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("42"),     "should contain value: {xml}");
        assert!(xml.contains("fileid"), "should contain name:  {xml}");
    }
}
