//! Low-level database helpers for `oc_filecache`, `oc_filecache_extended`,
//! and `oc_storages`.
//!
//! All queries are parameterised and use the table prefix from `NcDavState`.
//! This file is the public façade — implementation lives in `row/*.rs`.

pub mod comments;
pub mod counts;
pub mod extended;
pub mod favorites;
pub mod filecache;
pub mod paths;
pub mod properties;
pub mod propfind;
pub mod quota;
pub mod rekey;
pub mod sharing;
pub mod sql;
pub mod storage;
pub mod system_tags;
pub mod types;
pub mod workspace;

#[cfg(test)]
mod tests;

pub use comments::{build_comments_href, comments_counts_batch, get_comments_count, get_comments_unread};
pub use counts::{count_children, count_children_batch};
pub use extended::{get_extended, list_extended_batch};
pub use favorites::{get_favorite_fileids, lookup_by_ids};
pub use filecache::{list_changed_since, list_children, list_children_with_ext, lookup_by_id, lookup_by_path, lookup_by_path_with_ext};
pub use paths::{dav_to_fc_path, disk_path, path_hash};
pub use properties::{
    custom_properties_batch, delete_custom_properties_for_dir, delete_custom_properties_for_path,
    delete_custom_property, format_property_path, list_custom_properties, parse_clark_notation,
    update_custom_properties_path, update_custom_properties_path_subtree, upsert_custom_property,
};
pub use propfind::{PropfindCte, PropfindGates, propfind_batch_cte};
pub use quota::quota_free_space;
pub use rekey::rekey_subtree_paths;
pub use sharing::{
    apply_sharing_mask, compute_share_permissions, format_share_types_xml,
    format_sharees_xml, get_share_details, get_share_max_permissions, get_share_note,
    permissions_to_ocm_json, share_details_and_notes_batch, ShareDetail,
};
pub use storage::{get_storage_string_id, get_storage_string_id_cached, lookup_storage_id, SharedStorageCache};
pub use system_tags::{
    format_system_tags_xml, get_system_tags_for_file, system_tags_batch, SystemTagRow,
};
pub use types::{FileCacheExtRow, FileCacheRow};
pub use workspace::{get_metadata_json, get_user_preference, get_workspace_file};
