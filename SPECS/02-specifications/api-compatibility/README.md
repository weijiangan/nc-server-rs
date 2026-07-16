# API Compatibility — Sectioned

Split from the original `API_COMPATIBILITY.md` (Nextcloud Core API Compatibility Notes). Each section is its own file for focused reading.

## Overview

This document captures the minimum behavior needed to provide an API-compatible
core reimplementation (e.g. Rust) without breaking Nextcloud clients. It focuses
on public HTTP entry points, response envelopes, and cross-cutting behaviors that
clients and apps depend on. A full reimplementation requires matching the APIs
registered by core apps (see “Core app API surfaces” below).

## Sections

- [`01-scope.md`](01-scope.md) — Scope
- [`02-primary-entry-points.md`](02-primary-entry-points.md) — Primary entry points
- [`03-request-lifecycle-and-global-behavior.md`](03-request-lifecycle-and-global-behavior.md) — Request lifecycle and global behavior
- [`04-routing-and-url-structure.md`](04-routing-and-url-structure.md) — Routing and URL structure
- [`05-ocs-api-compatibility.md`](05-ocs-api-compatibility.md) — OCS API compatibility
- [`06-login-flows-and-oauth2.md`](06-login-flows-and-oauth2.md) — Login flows and OAuth2
- [`07-webdav-caldav-carddav.md`](07-webdav-caldav-carddav.md) — WebDAV, CalDAV, CardDAV
- [`08-security-and-request-validation.md`](08-security-and-request-validation.md) — Security and request validation
- [`09-public-status-endpoint.md`](09-public-status-endpoint.md) — Public status endpoint
- [`10-well-known-endpoints.md`](10-well-known-endpoints.md) — Well-known endpoints
- [`11-files-app-rest-api.md`](11-files-app-rest-api.md) — Files app REST API
- [`12-public-sharing-endpoints.md`](12-public-sharing-endpoints.md) — Public sharing endpoints
- [`13-app-compatibility-considerations.md`](13-app-compatibility-considerations.md) — App compatibility considerations
- [`14-configuration-values-influencing-api-behavior.md`](14-configuration-values-influencing-api-behavior.md) — Configuration values influencing API behavior
- [`15-reference-implementation-locations.md`](15-reference-implementation-locations.md) — Reference implementation locations

Back: [`../README.md`](../README.md)
