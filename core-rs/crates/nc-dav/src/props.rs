//! Nextcloud-specific DAV properties for the `{oc:}` and `{nc:}` namespaces.
//!
//! CONFORMANCE FIXES (vs original Phase 4 implementation):
//! - Removed bogus `oc:has-preview` (only `nc:has-preview` is valid, REQ §6.5)
//! - Fixed `{oc:}checksums` to wrap each entry in inner `<oc:checksum>` element
//! - Fixed `CK` double-emit when both UPDATE (bit 2) and CREATE (bit 4) set on dir
//! - Added `{oc:}data-fingerprint`, `{nc:}mount-type`, `{nc:}is-mount-root`,
//!   `{nc:}is-federated`, `{nc:}contained-folder-count`, `{nc:}contained-file-count`
//! - `build_props` now takes `data_fingerprint`, `child_dir_count`, `child_file_count`
//! - §9.5: `{oc:}favorite` and `{oc:}tags` are now populated from `oc_vcategory` /
//!   `oc_vcategory_to_object` instead of hardcoded to `"0"` / `""`
//!
//! ## PHASE-12.1 value discipline
//! Properties that have no value under PHP semantics (no checksum, no
//! direct-download URL, no share note, `upload_time` on directories,
//! `hide-download` off shared storage) are OMITTED rather than emitted
//! empty — the patched dav-server then groups them into the 404 propstat,
//! exactly matching PHP's null-returning handlers. `acl-can-*` /
//! `remind-me-at` are not PHP-core properties and are never emitted.
//!
//! ## References
//! - REQ §4.7 — Standard DAV properties
//! - REQ §4.8 — `{oc:}` namespace properties
//! - REQ §4.9 — `{nc:}` namespace properties
//! - REQ §6.5.1 — Favorites & personal tags properties

use dav_server::fs::DavProp;

use crate::metadata::NcMetaData;
use crate::tags;

// ─── Namespaces ───────────────────────────────────────────────────────────────

pub const OC_NS: &str = "http://owncloud.org/ns";
pub const NC_NS: &str = "http://nextcloud.org/ns";
/// OCS (Open Collaboration Services) namespace — used for `{ocs:}share-permissions`.
pub const OCS_NS: &str = "http://open-collaboration-services.org/ns";
/// OCM (Open Cloud Mesh) namespace — used for `{ocm:}share-permissions`.
pub const OCM_NS: &str = "http://open-cloud-mesh.org/ns";

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
/// - `is_mounted`: `true` when the file lives on a non-home mount (i.e.
///   `oc_storages.id` does not start with `"home::"`).  Adds the `M` flag to
///   `{oc:}permissions` (PHASE-7.6).
/// - `share_permissions`: MAX permissions bitmask from `oc_share` for this
///   file and owner; pass `31` when the file has no share rows (PHASE-7.6).
/// - `download_url`: direct-download URL for home-storage files, built from
///   `overwrite.cli.url` + `/remote.php/webdav/{path}`; pass `""` for
///   non-home storage (PHASE-7.6).
/// - `is_shared`: `true` when the current user is accessing this node via a
///   share (i.e. is a recipient, not the owner). Adds the `S` flag to
///   `{oc:}permissions` (PHASE-5, REQ §6.5).
/// - `note`: most-recent non-empty `oc_share.note` for this file (PHASE-7.6).
/// - `has_preview`: computed from mimetype + preview config (§10.12).
/// - `tags`: list of non-favorite tag names for `{oc:}tags` (§9.5).
/// - `favorite`: whether the file is favorited for `{oc:}favorite` (§9.5).
pub fn build_props(
    meta: &NcMetaData,
    instance_id: &str,
    uid: &str,
    owner_display_name: &str,
    do_content: bool,
    data_fingerprint: &str,
    child_dir_count: i64,
    child_file_count: i64,
    is_mounted: bool,
    is_shared: bool,
    share_permissions: i32,
    download_url: &str,
    note: &str,
    has_preview: bool,
    tags: &[String],
    favorite: bool,
) -> Vec<DavProp> {
    if !do_content {
        return prop_names();
    }

    // can_rename: matches PHP DavUtil::canRename() — true when updateable
    // (PERMISSION_UPDATE), or deletable + parent creatable (not checked here).
    let can_rename = meta.permissions & 2 != 0;
    let perms_str = encode_permissions(
        meta.permissions,
        meta.is_dir_flag,
        is_mounted,
        is_shared,
        can_rename,
    );

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
    let is_mount_root = matches!(meta.path.as_deref(), Some("") | Some("files"));
    let is_mount_root_str = if is_mount_root { "true" } else { "false" };

    // {DAV:}displayname — PHP: FilesPlugin.php:470-472 emits $node->getName(),
    // which is FileInfo::getName() (FileInfo.php:136-139): the cached name in
    // oc_filecache.name, or basename(path) when empty.  For the home DAV root
    // the cache name is "files" but PHP's FileInfo::getName() returns the UID
    // here because View::getFileInfo() overrides the root's name to basename(path)
    // only when internalPath is empty and the cached name is non-empty — the
    // net result is UID at the mount root.  Mirror that: mount-root → uid first,
    // then cached name, then basename(path).
    // PHASE-12.2.
    let displayname_val: &str = if is_mount_root {
        uid
    } else if !meta.display_name.is_empty() {
        meta.display_name.as_str()
    } else {
        meta.path
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .unwrap_or(uid)
    };

    // {nc:}mount-type — PHP: FilesPlugin.php:404-406 →
    // MountPoint::getMountType():
    //   home mounts      → ""       (MountPoint.php:268-270)
    //   shared mounts    → "shared" (files_sharing/lib/SharedMount.php:184-186)
    //   external storage → "external" / "external-session"
    //                      (files_external/lib/Config/ExternalMountPoint.php:27-29)
    //   group folders    → "group"  (groupfolders app)
    // The rewrite serves home and shared mounts today; external/group mounts
    // have no storage support yet, so they cannot reach this code path.
    // PHASE-12.8.
    let mount_type = if is_shared { "shared" } else { "" };

    let mut props = vec![
        // ── oc: ──────────────────────────────────────────────────────────
        make_prop("id", "oc", OC_NS, &oc_id),
        make_prop("fileid", "oc", OC_NS, &meta.fileid.to_string()),
        make_prop("permissions", "oc", OC_NS, &perms_str),
        make_prop("size", "oc", OC_NS, &meta.size.to_string()),
        make_prop("owner-id", "oc", OC_NS, uid),
        // {oc:}owner-display-name — resolved from oc_users.displayname (REQ §6.5 / §4.8)
        make_prop("owner-display-name", "oc", OC_NS, owner_display_name),
        // NOTE: {oc:}etag is deliberately NOT emitted — PHP has no registration
        // for {http://owncloud.org/ns}etag on the files endpoint and never
        // includes it in PROPFIND responses (verified against PHP source and
        // wire captures). `{DAV:}getetag` is emitted by dav-server from the
        // quoted `DavMetaData::etag()`. PHASE-12.10.
        // {oc:}checksums / {oc:}downloadURL — emitted conditionally below
        // (PHASE-12.1): PHP returns null (→404 propstat) when there is no
        // checksum / direct-download URL.
        make_prop("data-fingerprint", "oc", OC_NS, data_fingerprint),
        // §9.5: tags / favorite are populated from oc_vcategory / oc_vcategory_to_object.
        // Tags are serialized as <oc:tags><oc:tag>...</oc:tag>...</oc:tags>.
        // Favorite is "1" or "0".
        make_prop("tags", "oc", OC_NS, &tags::format_tags_xml(tags)),
        make_prop(
            "favorite",
            "oc",
            OC_NS,
            if favorite { "1" } else { "0" },
        ),
        // {ocs:}share-permissions — per-share MAX permissions from oc_share;
        // defaults to 31 (all permissions) for the owner's own unshared file
        // (REQ §6.5 / §4.8, PHASE-7.6).
        make_prop(
            "share-permissions",
            "ocs",
            OCS_NS,
            &share_permissions.to_string(),
        ),
        // ── nc: ──────────────────────────────────────────────────────────
        // NOTE: nc:has-preview only — oc:has-preview does NOT exist (REQ §6.5)
        // §10.12: computed from mimetype + preview config, not hardcoded.
        make_prop(
            "has-preview",
            "nc",
            NC_NS,
            if has_preview { "true" } else { "false" },
        ),
        make_prop(
            "creation_time",
            "nc",
            NC_NS,
            &meta.creation_time.to_string(),
        ),
        // {nc:}upload_time — emitted conditionally below: PHP registers it
        // for File nodes only; directories get a 404 propstat (PHASE-12.1).
        make_prop("mount-type", "nc", NC_NS, mount_type),
        make_prop("is-mount-root", "nc", NC_NS, is_mount_root_str),
        make_prop("is-federated", "nc", NC_NS, "false"),
        // {nc:}hide-download — emitted conditionally below: PHP returns null
        // (→404) unless the node lives on shared storage (PHASE-12.1).
        make_prop(
            "contained-folder-count",
            "nc",
            NC_NS,
            &child_dir_count.to_string(),
        ),
        make_prop(
            "contained-file-count",
            "nc",
            NC_NS,
            &child_file_count.to_string(),
        ),
        make_prop("hidden", "nc", NC_NS, "false"),
        // {nc:}share-attributes — PHP json_encode()s the share attributes
        // (FilesPlugin.php:350-352): "[]" when empty, NOT the empty string.
        // TODO(PHASE-12): real per-share attributes.
        make_prop("share-attributes", "nc", NC_NS, "[]"),
        // {nc:}acl-can-* and {nc:}remind-me-at were removed in PHASE-12.1:
        // they do not exist in PHP core (they belong to the groupfolders /
        // deck apps). PHP answers requested-but-unknown properties with a
        // 404 propstat, which the patched dav-server now produces
        // automatically.
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
        // {DAV:}displayname — PHASE-12.2 (value computed above).
        make_prop("displayname", "D", "DAV:", displayname_val),
    ];

    // ── PHASE-12.1 value discipline ────────────────────────────────────────
    // Properties whose PHP handlers return null — or that PHP does not
    // register for this node type — are OMITTED here, so the patched
    // dav-server groups them into the 404 propstat, exactly matching PHP.
    // Emitting "" / 0 instead would place them in the 200 propstat.

    // {oc:}checksums — FilesPlugin.php:506-513: null (→404) without checksum.
    if !checksums_val.is_empty() {
        props.push(make_prop("checksums", "oc", OC_NS, &checksums_val));
    }
    // {oc:}downloadURL — FilesPlugin.php:491-496: false (→404) when there is
    // no direct-download URL (non-home storage).
    if !download_url.is_empty() {
        props.push(make_prop("downloadURL", "oc", OC_NS, download_url));
    }
    // {nc:}upload_time — FilesPlugin.php:515-517 sits in the File-only
    // branch: directories land in the 404 propstat.
    if !meta.is_dir_flag {
        props.push(make_prop(
            "upload_time",
            "nc",
            NC_NS,
            &meta.upload_time.to_string(),
        ));
    }
    // {nc:}hide-download — FilesPlugin.php:425-436: null (→404) unless the
    // node lives on shared storage.
    // TODO(PHASE-12): source the actual oc_share.hide_download bit; shared
    // nodes report "false" until then.
    if is_shared {
        props.push(make_prop("hide-download", "nc", NC_NS, "false"));
    }
    // {nc:}note — FilesPlugin.php:418-423: null (→404) when the share has
    // no note.
    if !note.is_empty() {
        props.push(make_prop("note", "nc", NC_NS, note));
    }

    // Conditionally include metadata_etag if present.
    if let Some(ref etag) = meta.metadata_etag {
        props.push(make_prop("metadata_etag", "nc", NC_NS, etag));
    }

    props
}

// ─── Phase 12 extended properties ──────────────────────────────────────────────

/// Grouped parameters for Phase 12 PROPFIND properties that are populated from
/// additional tables beyond `oc_filecache`.
pub struct Phase12PropCtx<'a> {
    /// `{ocm:}share-permissions` — JSON array of OCM permission strings.
    /// Matches PHP `FilesPlugin::ncPermissions2ocmPermissions()`.
    pub ocm_share_permissions: &'a str,
    /// Inner XML for `{oc:}share-types` (child `<oc:share-type>` elements).
    /// Empty when no shares → self-closing element.
    pub share_types_xml: &'a str,
    /// Inner XML for `{nc:}sharees` (child `<nc:sharee>` elements).
    pub sharees_xml: &'a str,
    /// `{oc:}comments-count` — number of top-level comments for the node.
    pub comments_count: i64,
    /// `{oc:}comments-unread` — unread comments for the requesting user.
    pub comments_unread: i64,
    /// `{oc:}comments-href` — URL to the comments DAV endpoint.
    pub comments_href: &'a str,
    /// Inner XML for `{nc:}system-tags` (child `<nc:system-tag>` elements).
    /// Empty when no tags → self-closing element.
    pub system_tags_xml: &'a str,
}

/// Append all Phase 12 extended properties to the prop list.
///
/// Separated from `build_props` so the existing signature and all its tests
/// stay untouched. Properties are appended with PHP-semantics: empty strings
/// produce the correct self-closing elements (e.g. `<oc:share-types/>`).
pub fn add_phase12_props(props: &mut Vec<DavProp>, ctx: &Phase12PropCtx<'_>) {
    // {ocm:}share-permissions — JSON array (PHASE-12.4).
    props.push(make_prop(
        "share-permissions",
        "ocm",
        OCM_NS,
        ctx.ocm_share_permissions,
    ));

    // {oc:}share-types — always 200, self-closing when empty (PHASE-12.5).
    props.push(make_prop("share-types", "oc", OC_NS, ctx.share_types_xml));

    // {nc:}sharees — always 200, self-closing when empty (PHASE-12.5).
    props.push(make_prop("sharees", "nc", NC_NS, ctx.sharees_xml));

    // {oc:}comments-count — always int (PHASE-12.6).
    props.push(make_prop(
        "comments-count",
        "oc",
        OC_NS,
        &ctx.comments_count.to_string(),
    ));

    // {oc:}comments-unread — always int (PHASE-12.6).
    props.push(make_prop(
        "comments-unread",
        "oc",
        OC_NS,
        &ctx.comments_unread.to_string(),
    ));

    // {oc:}comments-href — only when non-empty (PHASE-12.6).
    if !ctx.comments_href.is_empty() {
        props.push(make_prop(
            "comments-href",
            "oc",
            OC_NS,
            ctx.comments_href,
        ));
    }

    // {nc:}system-tags — always 200, self-closing when empty (PHASE-12.7).
    props.push(make_prop(
        "system-tags",
        "nc",
        NC_NS,
        ctx.system_tags_xml,
    ));
}

// ─── Permission encoding ──────────────────────────────────────────────────────

/// Encode an `oc_filecache.permissions` bitmask to the Nextcloud permission
/// string used in `{oc:}permissions`.
///
/// Matches PHP `DavUtil::getDavPermissions()` (lib/public/Files/DavUtil.php).
///
/// Bitmask constants (from `\OCP\Constants`):
/// - `1`  = READ
/// - `2`  = UPDATE
/// - `4`  = CREATE
/// - `8`  = DELETE
/// - `16` = SHARE
///
/// Encoded characters (in PHP emission order):
/// - `S`  = shared (current user is a share recipient, not the owner)
/// - `R`  = shareable (owner has PERMISSION_SHARE)
/// - `M`  = mounted (file lives on a non-home storage)
/// - `G`  = readable (PERMISSION_READ)
/// - `D`  = deletable (PERMISSION_DELETE)
/// - `N`  = renamable
/// - `V`  = updateable / movable (PERMISSION_UPDATE)
/// - `W`  = writable (files only: PERMISSION_UPDATE set)
/// - `CK` = creatable (directories only: PERMISSION_CREATE set, signals
///          files can be created inside)
pub fn encode_permissions(
    perms: i32,
    is_dir: bool,
    is_mounted: bool,
    is_shared: bool,
    can_rename: bool,
) -> String {
    let mut p = String::new();

    // 1. Shared flag — current user is accessing via a share (not the owner).
    if is_shared {
        p.push('S');
    }

    // 2. Shareable — owner can share this node.
    if perms & 16 != 0 {
        p.push('R');
    }

    // 3. Mounted — file lives on a non-home storage.
    if is_mounted {
        p.push('M');
    }

    // 4. Readable.
    if perms & 1 != 0 {
        p.push('G');
    }

    // 5. Deletable.
    if perms & 8 != 0 {
        p.push('D');
    }

    // 6. Renamable — true when updateable, or deletable + parent creatable.
    if can_rename {
        p.push('N');
    }

    // 7. Updateable / movable.
    if perms & 2 != 0 {
        p.push('V');
    }

    // 8. Writable (files) or Creatable (dirs).
    //
    // NOTE (see SPECS/02-specifications/improvements.md §I.9): PHP
    // `DavUtil::getDavPermissions()` special-cases `W` for the root of a
    // movable mount — it re-derives writability from the underlying storage's
    // root cache entry instead of the node's own UPDATE bit, because the mount
    // layer artificially inflates a mount root with UPDATE so it can be
    // renamed/moved. That only affects a single-file share (a file mounted at
    // its own root). We use the persisted `oc_filecache.permissions` directly,
    // which holds the real granted mask (the inflation is a PHP runtime-only
    // value), so this is correct until Rust gains native share mounts that
    // replicate the inflation.
    if is_dir {
        if perms & 4 != 0 {
            p.push_str("CK");
        }
    } else if perms & 2 != 0 {
        p.push('W');
    }

    p
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
            name: name.to_string(),
            prefix: Some(prefix.to_string()),
            namespace: Some(ns.to_string()),
            xml: None,
        }
    }

    vec![
        name_only("id", "oc", OC_NS),
        name_only("fileid", "oc", OC_NS),
        name_only("permissions", "oc", OC_NS),
        name_only("size", "oc", OC_NS),
        name_only("owner-id", "oc", OC_NS),
        name_only("owner-display-name", "oc", OC_NS),
        name_only("checksums", "oc", OC_NS),
        name_only("data-fingerprint", "oc", OC_NS),
        name_only("downloadURL", "oc", OC_NS),
        name_only("tags", "oc", OC_NS),
        name_only("favorite", "oc", OC_NS),
        name_only("share-permissions", "ocs", OCS_NS),
        name_only("share-permissions", "ocm", OCM_NS),
        name_only("share-types", "oc", OC_NS),
        name_only("comments-count", "oc", OC_NS),
        name_only("comments-href", "oc", OC_NS),
        name_only("comments-unread", "oc", OC_NS),
        name_only("has-preview", "nc", NC_NS),
        name_only("creation_time", "nc", NC_NS),
        name_only("upload_time", "nc", NC_NS),
        name_only("metadata_etag", "nc", NC_NS),
        name_only("mount-type", "nc", NC_NS),
        name_only("is-mount-root", "nc", NC_NS),
        name_only("is-federated", "nc", NC_NS),
        name_only("hide-download", "nc", NC_NS),
        name_only("contained-folder-count", "nc", NC_NS),
        name_only("contained-file-count", "nc", NC_NS),
        name_only("note", "nc", NC_NS),
        name_only("hidden", "nc", NC_NS),
        name_only("share-attributes", "nc", NC_NS),
        name_only("sharees", "nc", NC_NS),
        name_only("system-tags", "nc", NC_NS),
        // {nc:}acl-can-* and {nc:}remind-me-at are not PHP-core properties
        // (PHASE-12.1) — removed.
        name_only("displayname", "D", "DAV:"),
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
            fileid: 42,
            size: 100,
            mtime: 1700000000,
            is_dir_flag: false,
            mime_type: "text/plain".into(),
            etag: Some("abc".into()),
            permissions: 27,
            creation_time: 1700000000,
            upload_time: 1700000000,
            checksum: checksum.map(String::from),
            display_name: "test.txt".into(),
            metadata_etag: None,
            storage: 1,
            path: Some("files/test.txt".into()),
            parent: 2,
        }
    }

    /// Call `build_props` with sensible defaults.  Extra params added in §4.8,
    /// §4.11, and §7.6: `owner_display_name` as `"Alice Test"`; `sync_token = None`;
    /// `is_mounted = false`; `is_shared = false`; `share_permissions = 31`; `download_url = ""`; `note = ""`.
    /// §9.5: `tags` = empty, `favorite` = false.
    fn build(meta: &NcMetaData) -> Vec<DavProp> {
        build_props(
            meta,
            "inst",
            "alice",
            "Alice Test",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",
            false,
            &[],
            false,
        )
    }

    // ── encode_permissions ────────────────────────────────────────────────────

    #[test]
    fn permissions_file_all() {
        // perms=31 (READ|UPDATE|CREATE|DELETE|SHARE), file, not mounted, not shared, can_rename
        // PHP: R(share) G(read) D(delete) N(renamable) V(updateable) W(writable) = "RGDNVW"
        let s = encode_permissions(31, false, false, false, true);
        assert_eq!(s, "RGDNVW", "perms=31 file: expected RGDNVW, got {s}");
    }

    #[test]
    fn permissions_dir_all() {
        // perms=31, dir, not mounted, not shared, can_rename
        // PHP: R G D N V CK = "RGDNVCK"
        let s = encode_permissions(31, true, false, false, true);
        assert_eq!(s, "RGDNVCK", "perms=31 dir: expected RGDNVCK, got {s}");
    }

    #[test]
    fn permissions_dir_value_15_encodes_gdnvck() {
        // Pure encoding fact: a directory whose effective permissions are 15
        // (SHARE bit absent) has no 'R' flag → "GDNVCK".  This is the value the
        // home root takes ONLY when sharing is disabled (apply_sharing_mask);
        // in the normal sharing-enabled case the home root is 31 → "RGDNVCK"
        // (see permissions_dir_all and the note in filesystem.rs::get_props).
        let s = encode_permissions(15, true, false, false, true);
        assert_eq!(s, "GDNVCK", "perms=15 dir: expected GDNVCK, got {s}");
    }

    #[test]
    fn permissions_shared_read_only_file() {
        // perms=1 (READ only), file, not mounted, is_shared=true, can_rename=false
        // PHP: S G = "SG"
        let s = encode_permissions(1, false, false, true, false);
        assert_eq!(s, "SG", "shared read-only file: expected SG, got {s}");
    }

    #[test]
    fn permissions_share_flag_is_r_not_s() {
        // The SHARE permission (bit 16) is encoded as 'R' (shareable), not 'S' (shared).
        // 'S' means the current user is a share recipient (is_shared=true).
        let s = encode_permissions(16, false, false, false, false);
        assert_eq!(s, "R", "SHARE bit alone should produce R, got {s}");
    }

    #[test]
    fn permissions_file_no_create_flag() {
        // perms=6 (UPDATE|CREATE), file — CREATE bit on file should NOT produce CK.
        // PHP: N V W = "NVW"
        let s = encode_permissions(6, false, false, false, true);
        assert_eq!(s, "NVW", "perms=6 file: expected NVW, got {s}");
        assert_eq!(
            s.matches("CK").count(),
            0,
            "CK must not appear on files: {s}"
        );
    }

    #[test]
    fn permissions_dir_create_without_update() {
        // perms=4 (CREATE only), dir, can_rename=false (no UPDATE)
        // PHP: G (READ) not set, DELETE not set, no UPDATE... → just "CK"
        // Actually: no READ, no DELETE, no UPDATE, no SHARE... just CREATE on dir = "CK"
        let s = encode_permissions(4, true, false, false, false);
        assert_eq!(s, "CK", "perms=4 dir: expected CK, got {s}");
    }

    #[test]
    fn permissions_dir_no_create() {
        // perms=1 (READ only), dir, not mounted, not shared, can_rename=false
        // PHP: G only = "G"
        let s = encode_permissions(1, true, false, false, false);
        assert_eq!(s, "G", "perms=1 dir: expected G, got {s}");
    }

    #[test]
    fn permissions_dir_update_without_create() {
        // perms=2 (UPDATE only), dir — PHP does NOT add CK here.
        // PHP: N V = "NV"
        let s = encode_permissions(2, true, false, false, true);
        assert_eq!(s, "NV", "perms=2 dir: expected NV, got {s}");
        assert!(
            !s.contains("CK"),
            "CK must NOT be added for dir UPDATE without CREATE: {s}"
        );
    }

    #[test]
    fn permissions_dir_update_and_create() {
        // perms=6 (UPDATE|CREATE), dir, can_rename=true
        // PHP: N V CK = "NVCK"
        let s = encode_permissions(6, true, false, false, true);
        assert_eq!(s, "NVCK", "perms=6 dir: expected NVCK, got {s}");
        assert_eq!(
            s.matches("CK").count(),
            1,
            "CK should appear exactly once: {s}"
        );
    }

    // ── is_mounted / M flag (PHASE-7.6) ──────────────────────────────────────

    #[test]
    fn permissions_m_flag_when_mounted() {
        let s = encode_permissions(1, false, true, false, false);
        assert!(
            s.contains('M'),
            "M flag must be present when is_mounted=true: {s}"
        );
        assert_eq!(s, "MG", "perms=1 mounted file: expected MG, got {s}");
    }

    #[test]
    fn permissions_no_m_flag_when_not_mounted() {
        let s = encode_permissions(31, false, false, false, true);
        assert!(
            !s.contains('M'),
            "M flag must NOT be present for home storage: {s}"
        );
    }

    #[test]
    fn permissions_m_flag_in_full_perms_string() {
        // perms=31, file, mounted — PHP encoding order: R M G D N V W
        let s = encode_permissions(31, false, true, false, true);
        assert_eq!(
            s, "RMGDNVW",
            "perms=31 mounted file: expected RMGDNVW, got {s}"
        );
    }

    // ── owner-display-name (§4.8) ─────────────────────────────────────────────

    #[test]
    fn owner_display_name_uses_provided_value() {
        let props = build(&test_meta(None));
        let p = props
            .iter()
            .find(|p| p.name == "owner-display-name")
            .unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("Alice Test"),
            "should contain display name: {xml}"
        );
    }

    #[test]
    fn owner_display_name_not_same_as_uid_when_different() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "alice",
            "Alice Test",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = props
            .iter()
            .find(|p| p.name == "owner-display-name")
            .unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(!xml.contains(">alice<"), "must not contain raw uid: {xml}");
    }

    // ── downloadURL (§4.8 / PHASE-7.6) ───────────────────────────────────────────

    #[test]
    fn download_url_omitted_when_not_provided() {
        // PHASE-12.1: PHP returns false (→404 propstat) when there is no
        // direct-download URL — the property must be omitted, not empty.
        let props = build(&test_meta(None));
        assert!(
            props
                .iter()
                .all(|p| !(p.name == "downloadURL" && p.namespace.as_deref() == Some(OC_NS))),
            "{{oc:}}downloadURL must be omitted when no URL is available"
        );
    }

    #[test]
    fn download_url_set_when_provided() {
        let url = "https://nc.example.com/remote.php/webdav/Photos/img.jpg";
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            url,
            "",            false,
            &[],
            false,
        );
        let p = props
            .iter()
            .find(|p| p.name == "downloadURL" && p.namespace.as_deref() == Some(OC_NS))
            .expect("{oc:}downloadURL must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains(url),
            "downloadURL must contain the provided URL: {xml}"
        );
    }

    // ── share-permissions real value (PHASE-7.6) ──────────────────────────────

    #[test]
    fn share_permissions_reflects_passed_value() {
        // A file shared read-only has permissions=1 in oc_share
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            1,
            "",
            "",            false,
            &[],
            false,
        );
        let p = props
            .iter()
            .find(|p| p.name == "share-permissions" && p.namespace.as_deref() == Some(OCS_NS))
            .expect("{ocs:}share-permissions must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains(">1<"),
            "share-permissions must reflect passed value 1: {xml}"
        );
    }

    // ── note (PHASE-7.6) ──────────────────────────────────────────────────────

    #[test]
    fn note_omitted_by_default() {
        // PHASE-12.1: PHP returns null (→404 propstat) when the share has no
        // note — the property must be omitted, not empty.
        let props = build(&test_meta(None));
        assert!(
            props.iter().all(|p| p.name != "note"),
            "{{nc:}}note must be omitted when empty"
        );
    }

    #[test]
    fn note_propagated_when_provided() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "hello share note",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "note").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("hello share note"),
            "note must contain provided text: {xml}"
        );
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
        assert!(
            xml.contains("31"),
            "default share-permissions should be 31: {xml}"
        );
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

    // ── checksums ─────────────────────────────────────────────────────────────

    #[test]
    fn checksums_wraps_in_checksum_element() {
        let props = build_props(
            &test_meta(Some("SHA1:abc123")),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "checksums").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("oc:checksum"), "missing inner element: {xml}");
        assert!(xml.contains("SHA1:abc123"), "missing value: {xml}");
    }

    #[test]
    fn checksums_empty_when_no_checksum() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        // PHASE-12.1: PHP returns null (→404 propstat) when there is no
        // checksum — the property must be omitted, not empty.
        assert!(
            props.iter().all(|p| p.name != "checksums"),
            "{{oc:}}checksums must be omitted when no checksum is stored"
        );
    }

    // ── namespace correctness ─────────────────────────────────────────────────

    #[test]
    fn no_oc_has_preview_only_nc_has_preview() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let oc_pv = props
            .iter()
            .find(|p| p.name == "has-preview" && p.namespace.as_deref() == Some(OC_NS));
        let nc_pv = props
            .iter()
            .find(|p| p.name == "has-preview" && p.namespace.as_deref() == Some(NC_NS));
        assert!(oc_pv.is_none(), "oc:has-preview must not be emitted");
        assert!(nc_pv.is_some(), "nc:has-preview must be emitted");
    }

    // ── new required props ────────────────────────────────────────────────────

    // ── is-mount-root (§4.9) ──────────────────────────────────────────────

    fn test_meta_with_path(path: Option<&str>) -> NcMetaData {
        NcMetaData {
            fileid: 1,
            size: 0,
            mtime: 0,
            is_dir_flag: true,
            mime_type: "httpd/unix-directory".into(),
            etag: None,
            permissions: 31,
            creation_time: 0,
            upload_time: 0,
            checksum: None,
            display_name: "".into(),
            metadata_etag: None,
            storage: 1,
            path: path.map(String::from),
            parent: 0,
        }
    }

    #[test]
    fn is_mount_root_true_for_files_path() {
        let meta = test_meta_with_path(Some("files"));
        let props = build_props(
            &meta, "inst", "u", "U", true, "", 0, 0, false, false, 31, "", "",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("true"),
            "is-mount-root must be true for path=files: {xml}"
        );
    }

    #[test]
    fn is_mount_root_true_for_empty_path() {
        let meta = test_meta_with_path(Some(""));
        let props = build_props(
            &meta, "inst", "u", "U", true, "", 0, 0, false, false, 31, "", "",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("true"),
            "is-mount-root must be true for path=\"\": {xml}"
        );
    }

    #[test]
    fn is_mount_root_false_for_subdir() {
        let meta = test_meta_with_path(Some("files/Photos"));
        let props = build_props(
            &meta, "inst", "u", "U", true, "", 0, 0, false, false, 31, "", "",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "is-mount-root").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("false"),
            "is-mount-root must be false for subdir: {xml}"
        );
    }

    // ── hide-download (§4.9) ──────────────────────────────────────────────────

    #[test]
    fn hide_download_omitted_for_non_shared() {
        // PHASE-12.1: PHP returns null (→404 propstat) unless the node lives
        // on shared storage (FilesPlugin.php:425-436).
        let props = build(&test_meta(None));
        assert!(
            props
                .iter()
                .all(|p| !(p.name == "hide-download" && p.namespace.as_deref() == Some(NC_NS))),
            "{{nc:}}hide-download must be omitted for non-shared nodes"
        );
    }

    #[test]
    fn hide_download_present_on_shared_nodes() {
        let meta = test_meta(None);
        let props = build_props(
            &meta, "inst", "u", "U", true, "", 0, 0, false, true, 31, "", "",            false,
            &[],
            false,
        );
        let p = props
            .iter()
            .find(|p| p.name == "hide-download" && p.namespace.as_deref() == Some(NC_NS))
            .expect("{nc:}hide-download must be present on shared nodes");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("false"),
            "{{nc:}}hide-download must be false (TODO: real value) for shared nodes: {xml}"
        );
    }

    #[test]
    fn hide_download_in_prop_names() {
        let meta = test_meta(None);
        let names = build_props(
            &meta, "inst", "u", "U", false, "", 0, 0, false, false, 31, "", "",            false,
            &[],
            false,
        );
        let found = names
            .iter()
            .any(|p| p.name == "hide-download" && p.namespace.as_deref() == Some(NC_NS));
        assert!(
            found,
            "{{nc:}}hide-download must appear in propnames response"
        );
    }

    #[test]
    fn data_fingerprint_present() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "fp123",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = props.iter().find(|p| p.name == "data-fingerprint").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("fp123"), "{xml}");
    }

    #[test]
    fn mount_type_matches_php_mount_point_type() {
        // PHP: FilesPlugin.php:404-406 → MountPoint::getMountType():
        // home mount → "" (MountPoint.php:268-270), shared mount → "shared"
        // (SharedMount.php:184-186). PHASE-12.8.
        let home = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = home.iter().find(|p| p.name == "mount-type").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert_eq!(
            xml,
            "<nc:mount-type xmlns:nc=\"http://nextcloud.org/ns\"></nc:mount-type>",
            "home mount must emit empty mount-type, got {xml}"
        );

        let shared = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            true,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = shared.iter().find(|p| p.name == "mount-type").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("shared"),
            "shared mount must emit 'shared', got {xml}"
        );
    }

    #[test]
    fn displayname_matches_php_getname() {
        // PHP: FilesPlugin.php:470-472 → FileInfo::getName() (FileInfo.php:
        // 136-139): cached name when non-empty, mount root falls back to the
        // UID in PHP's tree. PHASE-12.2.
        // Non-root: cached name wins.
        let props = build(&test_meta(None));
        let p = props.iter().find(|p| p.name == "displayname").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains(">test.txt<"), "{xml}");

        // Mount root with empty cache name (home roots have NULL name in
        // oc_filecache) → UID.
        let mut root_meta = test_meta(None);
        root_meta.display_name = String::new();
        root_meta.path = Some("files".into());
        root_meta.is_dir_flag = true;
        let props = build(&root_meta);
        let p = props.iter().find(|p| p.name == "displayname").unwrap();
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert_eq!(
            xml,
            "<D:displayname xmlns:D=\"DAV:\">alice</D:displayname>",
            "mount root must emit the UID, got {xml}"
        );
    }

    #[test]
    fn child_counts_present() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            3,
            7,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let dirs = props
            .iter()
            .find(|p| p.name == "contained-folder-count")
            .unwrap();
        let files = props
            .iter()
            .find(|p| p.name == "contained-file-count")
            .unwrap();
        let dirs_xml = std::str::from_utf8(dirs.xml.as_ref().unwrap()).unwrap();
        let files_xml = std::str::from_utf8(files.xml.as_ref().unwrap()).unwrap();
        assert!(dirs_xml.contains('3'), "{dirs_xml}");
        assert!(files_xml.contains('7'), "{files_xml}");
    }

    // ── DAV quota properties (REQ §6.5 / PHASE-4.7) ──────────────────────────────

    /// `{DAV:}quota-available-bytes` must be present with value "-3" (SPACE_UNLIMITED).
    /// dav-server suppresses its own emit when `get_quota()` returns `None` for total,
    /// so we inject it here without producing a duplicate.
    #[test]
    fn quota_available_bytes_is_minus_three() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        let p = props
            .iter()
            .find(|p| p.name == "quota-available-bytes" && p.namespace.as_deref() == Some("DAV:"))
            .expect("{DAV:}quota-available-bytes must be present");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(
            xml.contains("-3"),
            "quota-available-bytes must be -3 (SPACE_UNLIMITED): {xml}"
        );
    }

    /// `{DAV:}quota-available-bytes` must NOT appear under the OC or NC namespace.
    #[test]
    fn quota_available_bytes_not_in_oc_or_nc_namespace() {
        let props = build_props(
            &test_meta(None),
            "inst",
            "u",
            "U",
            true,
            "",
            0,
            0,
            false,
            false,
            31,
            "",
            "",            false,
            &[],
            false,
        );
        for p in &props {
            if p.name == "quota-available-bytes" {
                let ns = p.namespace.as_deref().unwrap_or("");
                assert_eq!(
                    ns, "DAV:",
                    "quota-available-bytes must be in DAV: namespace, got {ns}"
                );
            }
        }
    }

    // ── make_prop ─────────────────────────────────────────────────────────────

    #[test]
    fn make_prop_produces_xml() {
        let p = make_prop("fileid", "oc", OC_NS, "42");
        let xml = std::str::from_utf8(p.xml.as_ref().unwrap()).unwrap();
        assert!(xml.contains("42"), "should contain value: {xml}");
        assert!(xml.contains("fileid"), "should contain name:  {xml}");
    }
}
