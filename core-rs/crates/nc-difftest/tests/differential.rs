//! Differential integration tests. Gated behind `-- --ignored` / `NC_DIFFTEST=1`
//! so the default `cargo test --lib` never needs a live stack. Bring the stack
//! up with `make diff-up` first (Phase 16.1).
//!
//! Phase 16.2 ships the preconditions gate; the scenario-driven tests are added
//! in Phase 16.4+ (parametrized over `scenarios/*.yaml`).

#[tokio::test]
#[ignore = "requires a live SUT + oracle stack (`make diff-up`)"]
async fn preconditions_pass() {
    let cfg = nc_difftest::config::Config::from_env().expect("NC_DIFFTEST_* config");
    nc_difftest::preconditions::check(&cfg)
        .await
        .expect("preconditions should pass on a known-good stack");
}
