#![forbid(unsafe_code)]

pub mod capabilities;
pub mod envelope;
pub mod handlers;
pub mod router;

pub use capabilities::{load_capability_cache, SharedCapabilityCache};

use nc_db::appconfig::SharedAppConfigCache;

/// State threaded through all OCS handlers.
#[derive(Clone)]
pub struct OcsState {
    pub appconfig_cache: SharedAppConfigCache,
    pub capability_cache: SharedCapabilityCache,
}
