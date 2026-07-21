//! Native preview / thumbnail fast path (Phase 11).
//!
//! This crate implements the Rust-native preview serve path so that warm gallery
//! loads issue **zero** PHP-FPM requests and cold misses are generated through an
//! **isolated** out-of-process backend (Imaginary/libvips) — Rust never binds an
//! image codec in-process.
//!
//! Modules:
//! - [`size`] — PHP-exact size negotiation (`Generator::calculateSize`).
//! - [`store`] — `oc_previews` row model, md5-sharded byte-path construction, the
//!   in-memory max/match selection, and the row read query.
//! - [`response`] — response metadata: header / ETag / 304 / Cache-Control parity.
//! - [`concurrency`] — generation semaphore sizing + request coalescing.
//! - *(forthcoming)* provider registry (lifted from `nc-dav`), snowflake ids, the
//!   isolated generation backend, overwrite invalidation, and the HTTP handlers.
//!
//! Design: `SPECS/03-implementation-plan/plan/14-native-preview-thumbnail-fast-path.md`;
//! tasks: `SPECS/04-tasks/phase-11.md`.

#![forbid(unsafe_code)]

pub mod concurrency;
pub mod response;
pub mod size;
pub mod store;
