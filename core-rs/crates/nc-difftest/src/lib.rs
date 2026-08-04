//! `nc-difftest` — differential integration-test harness (Phase 16).
//!
//! Runs the same HTTP operation sequence against the Rust `nc-server` (SUT) and
//! a pure-PHP instance (Oracle), then diffs the resulting PostgreSQL state and
//! on-disk file tree. Any divergence is a bug signal.
//!
//! This crate is intentionally **black-box**: it speaks HTTP to the two live
//! instances and SQL to their two databases, and links **no** `nc-*` crate, so
//! it remains an independent oracle for their behavior.

pub mod canonicalize;
pub mod client;
pub mod config;
pub mod db;
pub mod delta;
pub mod fs;
pub mod preconditions;
pub mod report;
pub mod scenario;
