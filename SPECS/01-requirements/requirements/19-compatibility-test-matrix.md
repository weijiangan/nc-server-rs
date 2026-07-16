## 19. Compatibility Test Matrix

The following existing test suites serve as the acceptance criteria (no new test infrastructure needed for protocol compliance):

### Integration (Behat/Gherkin)

| Suite | Covers |
|---|---|
| `build/integration/features/maintenance-mode.feature` | Maintenance mode HTTP behavior |
| `build/integration/features/ocs-v1.feature` | OCS v1 envelope and endpoints |
| `build/integration/features/auth.feature` | All auth flows |
| `build/integration/capabilities_features/capabilities.feature` | Capabilities endpoint |
| `build/integration/dav_features/webdav-related.feature` | WebDAV v1 |
| `build/integration/dav_features/dav-v2.feature` | WebDAV v2, chunking v2 |
| `build/integration/dav_features/dav-v2-public.feature` | Public share DAV |
| `build/integration/dav_features/principal-property-search.feature` | Principal lookup |
| `build/integration/files_features/checksums.feature` | Checksum upload/download |
| `build/integration/files_features/download.feature` | File download |
| `build/integration/files_features/metadata.feature` | File metadata |
| `build/integration/files_features/tags.feature` | Tagging via DAV |
| `build/integration/files_features/transfer-ownership.feature` | Ownership transfer |
| `build/integration/ratelimiting_features/ratelimiting.feature` | Rate limiting / brute-force |
| `build/integration/routing_features/apps-and-routes.feature` | Route resolution |
| `build/integration/sharing_features/*.feature` | Sharing (PHP-FPM proxied) |
| `build/integration/features/provisioning-v1.feature` | Provisioning API (PHP-FPM proxied) |
| `build/integration/features/provisioning-v2.feature` | Provisioning API v2 (PHP-FPM proxied) |

### UI / End-to-End (Cypress)

| Suite | Covers |
|---|---|
| `cypress/e2e/files/*.cy.ts` | Files app UI: navigation, upload, download, rename, delete, search, sort, settings |
| `cypress/e2e/core/*.cy.ts` | Core platform behavior |

### Unit tests (PHP, as behavior oracles)

| Path | Use |
|---|---|
| `apps/dav/tests/unit/**` | Reference for exact DAV property values and edge cases |
| `tests/Core/**` | Reference for auth, OCS, and config behavior |

---

---

Prev: [`18-configuration-file.md`](18-configuration-file.md) · Up: [`README.md`](README.md) · Next: [`20-non-functional-requirements.md`](20-non-functional-requirements.md)
