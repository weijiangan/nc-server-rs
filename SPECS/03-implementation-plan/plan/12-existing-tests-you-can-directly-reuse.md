## Existing tests you can directly reuse

### Integration (Behat/Gherkin) — highest value for protocol compatibility
- `build/integration/features/maintenance-mode.feature`
- `build/integration/features/ocs-v1.feature`
- `build/integration/features/auth.feature`
- `build/integration/features/provisioning-v1.feature`
- `build/integration/features/provisioning-v2.feature`
- `build/integration/capabilities_features/capabilities.feature`
- `build/integration/dav_features/dav-v2.feature`
- `build/integration/dav_features/dav-v2-public.feature`
- `build/integration/dav_features/webdav-related.feature`
- `build/integration/dav_features/principal-property-search.feature`
- `build/integration/files_features/checksums.feature`
- `build/integration/files_features/download.feature`
- `build/integration/files_features/metadata.feature`
- `build/integration/files_features/tags.feature`
- `build/integration/files_features/transfer-ownership.feature`
- `build/integration/ratelimiting_features/ratelimiting.feature`
- `build/integration/routing_features/apps-and-routes.feature`
- `build/integration/sharing_features/*.feature`

### UI/E2E (Cypress) — verifies real client behavior through files app
- `cypress/e2e/files/*.cy.ts`
- `cypress/e2e/core/*.cy.ts` (basic platform checks)

### Unit tests in PHP codebase (reference behavior)
- `apps/dav/tests/unit/**`
- `tests/Core/**`

Use these as behavior oracles while implementing Rust handlers; only write new tests where no existing suite covers the Rust-specific concurrency/state behavior.

---

---

Prev: [`11-load-validation-and-starvation-regression-test.md`](11-load-validation-and-starvation-regression-test.md) · Up: [`README.md`](README.md) · Next: [`13-future-considerations-architectural-evolution.md`](13-future-considerations-architectural-evolution.md)
