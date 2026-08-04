# 17. CalDAV/CardDAV: build-vs-adopt exploration

Status: **exploration / decision record — no phase scheduled.** Findings below are
grounded against the PHP source (`workspace/server/`), the live dev database, the
vendored `dav-server` source, and a local checkout of RustiCal (`~/Git/rustical`,
v0.14.1) as of 2026-08-03. Effort numbers are engineering estimates, not
measurements, and are labelled as such.

## Context — the question

What is the effort to add CalDAV/CardDAV on top of the vendored `dav-server`
crate, and should it be built in-house or adopted from an existing Rust server
(RustiCal)?

### Where the project stands today (verified)

- CalDAV/CardDAV is **explicitly out of scope** and delegated to PHP-FPM:
  `SPECS/01-requirements/requirements/01-scope.md` ("Out of scope … `dav`
  CalDAV/CardDAV stacks"). It is not part of any phase in `SPECS/04-tasks/`.
- The router already wires the delegation, per subtree
  (`core-rs/crates/nc-server/src/router.rs:181-187`, mirrored for `/dav/*` at
  `:200-205`): `/remote.php/dav/calendars|addressbooks|principals|public-calendars|system-calendars`
  → `php_fpm_fallback`. Only `/dav/files/{uid}` and `/dav/uploads/{uid}` are native.
- The shim routes the legacy entry points to sabre
  (`core-rs/php-shim/index.php:339-346`): `caldav`/`calendar` →
  `dav/appinfo/v1/caldav.php`, `carddav`/`contacts` → `dav/appinfo/v1/carddav.php`.
- So CalDAV/CardDAV **works today via delegation**; the question is purely about
  if/when/how to take it native.

## What "CalDAV/CardDAV for Nextcloud" actually means (PHP ground truth)

### It is database-backed, not file-backed

Calendar objects and cards are blobs in PostgreSQL, with indexed metadata around
them. Tables present in the live dev DB (`master-database-pgsql-1`):

| Area | Tables |
| --- | --- |
| CalDAV core | `oc_calendars`, `oc_calendarobjects`, `oc_calendarobjects_props`, `oc_calendarchanges` |
| Scheduling/invitations | `oc_schedulingobjects`, `oc_calendar_invitations` |
| Extensions | `oc_calendar_reminders`, `oc_calendarsubscriptions`, `oc_calendars_federated`, `oc_calendar_resources`(+`_md`), `oc_calendar_rooms`(+`_md`) |
| CardDAV core | `oc_addressbooks`, `oc_cards`, `oc_cards_properties`, `oc_addressbookchanges` |
| DAV plumbing | `oc_dav_shares`, `oc_dav_absence`, `oc_dav_cal_proxy` |

Every write has fan-out. Verified in `apps/dav/lib/CalDAV/CalDavBackend.php`:
the props index table (`:204`), `calendarchanges` inserts on mutation
(`:2984-3006`, "Adds a change record to the calendarchanges table"), and
`schedulingobjects` maintenance (`:2853-2971`). A native PUT is not one row — it
is the object blob plus props-index rewrites, a change record (feeding RFC 6578
sync), and scheduling-object updates. This is the same hidden-side-effect failure
mode documented in CLAUDE.md (engineering hygiene rules 3, 4, 6, 7).

### The client-sync workhorse is RFC 6578, and only the cal/card trees speak it

`SPECS/02-specifications/api-compatibility/07-webdav-caldav-carddav.md`
("WebDAV delta sync") documents that `sync-collection` REPORT /
`{DAV:}sync-token` are implemented **only** by the CalDAV/CardDAV trees (the
files connector deliberately does not). Real clients (DAVx5, Thunderbird, Apple,
GNOME/Evolution) rely on it. Any candidate implementation without sync-tokens is
a non-starter for parity.

### PHP surface size (measured)

- `apps/dav/lib/CalDAV`: **21,427 LOC** PHP (incl. Schedule/FreeBusy, Sharing,
  Publishing, BirthdayCalendar, Reminder, ResourceBooking, Trashbin, WebcalCaching,
  InvitationResponse, Federation, Proxy/delegation).
- `apps/dav/lib/CardDAV`: **5,239 LOC** PHP.
- Underneath: sabre/dav's own CalDAV+CardDAV plugin layer
  (`3rdparty/sabre/dav/lib/{CalDAV,CardDAV}`): **~14,972 LOC** PHP, plus the
  `sabre/vobject` iCalendar/vCard parser.

### Parity-relevant advertised surface

The DAV header PHP advertises (captured in `comparison.md`) includes
`calendarserver-principal-property-search`, `nc-calendar-search`,
`nc-enable-birthday-calendar` — i.e. the *absence* of native cal/card support is
still client-visible through capabilities and headers even while the subtrees are
delegated. Group principals, public calendars, and system calendars (birthday
calendar synthesized from addressbooks) are all part of the tree clients see.

## Candidate A — the vendored `dav-server` crate's `caldav`/`carddav` features

The vendored crate (`core-rs/vendor/dav-server/`, v0.11.0 fork, branch
`nextcloud-0.11.0`, already carrying the PHASE-12.1 propstat-parity patches per
`core-rs/Cargo.toml:128-140`) *does* ship caldav/carddav modules
(`caldav.rs`, `carddav.rs`, `handle_caldav.rs`, `handle_carddav.rs`,
~1,640 LOC), wired at `davhandler.rs:554-565`. Verified findings:

- **Not currently compiled in.** Crate default features are `localfs,memfs`;
  `nc-dav` enables no extras, so the caldav/carddav code is dead weight today.
- **Wrong data model.** It treats calendar objects as `.ics` files on a
  `DavFileSystem`. `handle_calendar_query`
  (`vendor/dav-server/src/handle_caldav.rs:374-420`) does `read_dir` → `open`
  every child → read the whole blob → `is_calendar_data()` string sniff →
  in-memory `matches_query` over the raw text. PHP answers the same REPORT with
  one indexed SQL join over `oc_calendarobjects_props`. This violates the
  "one query when PHP does one query" principle on the hot sync path.
- **No RFC 6578 at all.** No sync-token / `sync-collection` implementation
  anywhere in the crate (verified by grep). It has `calendar-query`,
  `calendar-multiget`, `free-busy-query`, `MKCALENDAR` — and that is the full
  list.
- **No write-side effects.** No calendar-data validation on PUT, no props
  indexing, no change records, no scheduling.

**Verdict:** the vendored caldav layer is a file-based CalDAV (its upstream use
case). For this project it would save ~1–2 weeks of REPORT-body XML parsing and
multistatus writing, but its core loop contradicts the performance constitutions
and it lacks the one mechanism (sync-tokens) that makes cal/card clients work.
Using it as a foundation would mean gutting it. **Parts bin at best.**

## Candidate B — RustiCal (`~/Git/rustical`, v0.14.1)

Standalone axum-based CalDAV/CardDAV server. Workspace crates: `dav`, `caldav`,
`carddav`, `xml`, `ical`, `store`, `store_sqlite`, `dav_push`, `frontend`,
`oidc`.

### In its favour (verified)

- **License-compatible**: AGPL-3.0-or-later vs. this project's `AGPL-3.0-only`
  (`core-rs/Cargo.toml:18`). Vendoring parts of it is legally unproblematic.
- **The protocol surface the vendored crate lacks, with tests**: full
  `calendar-query` comp-filter/prop-filter engine
  (`crates/caldav/src/calendar/methods/report/calendar_query/`),
  `calendar-multiget`, `addressbook-query`/`addressbook-multiget`, and
  **`sync-collection` with sync-tokens** for both cal and card
  (`…/report/sync_collection.rs`, `crates/dav/src/extensions/synctoken.rs`).
  Advertised DAV header: `1, 3, access-control, calendar-access, webdav-push`
  (`crates/caldav/src/calendar/service.rs:61`).
- **Own iCalendar parser** (`crates/ical/`, with `chrono-tz` and RFC 7809
  timezones-by-reference per its README) plus WebDAV Push support — the two
  hardest components to get right from scratch.
- Tested against the clients that matter (README: DAVx5, Thunderbird, Apple
  Calendar, GNOME Calendar/Contacts, Evolution).

### Against (verified)

- **Its store layer models its own schema, not Nextcloud's.** The
  `CalendarReadStore`/`CalendarWriteStore` traits
  (`crates/store/src/calendar_store.rs`) are shaped around RustiCal's own
  SQLite design: string principals, uuid-style calendar ids, its own `Calendar`
  struct, its own trashbin/restore semantics. The only backend is
  `store_sqlite` — no Postgres. Adapting it to `oc_calendars`/`oc_calendarobjects`
  (+props/changes/scheduling fan-out, integer ids, `principaluri` strings,
  integer synctokens) means writing the entire storage layer anyway — and that
  storage layer is where the parity work lives.
- **Large NC-parity feature gaps.** Not present in RustiCal: CalDAV scheduling
  (inbox/outbox/free-busy/iTIP), invitations, reminders, NC's sharing model
  (`oc_dav_shares`), webcal subscriptions caching (subscription store exists but
  semantics differ), system calendars, birthday calendars, absence
  (`oc_dav_absence`), delegation/proxy (`oc_dav_cal_proxy`), resource/room
  booking, `nc-calendar-search`, group principals, public calendars.
- **Upstream risk**: single maintainer, v0.14, README warns "active
  development"; its roadmap (frontend, OIDC, its own auth) is orthogonal to
  this project. This repo already carries one vendored DAV fork; a second
  moving dependency doubles that carrying cost.

**Verdict:** not adoptable as a server or as a store. Potentially valuable as a
**reference implementation / selective vendor donor** for: comp-filter &
prop-filter matching semantics, sync-token arithmetic, and RFC 7809-aware
iCalendar parsing. Estimate of what harvesting saves: ~3–6 weeks of
protocol-correctness hunting.

## Effort estimate (labelled: estimates, not measurements)

Basis: measured PHP LOC above, the 19-table schema, and this project's parity
verification burden (A/B harness, PHP source tracing per CLAUDE.md).

| Slice | Contents | Estimate (1 engineer) |
| --- | --- | --- |
| **Core** — the minimum for real clients to sync | collections + objects, PROPFIND properties, multiget, calendar/addressbook-query against the indexed props tables, **sync-collection**, MKCALENDAR, PUT/DELETE with full write fan-out (props, changes, scheduling rows), etag/synctoken semantics matching PHP on Postgres | ~6–10 weeks |
| **Collaboration** | sharing (user/group/public/publish), subscriptions + webcal caching, system calendars, birthday calendar | +4–6 weeks |
| **Long tail** | scheduling inbox/outbox/free-busy, invitations, reminders, calendar trashbin/restore, resource & room booking, absence, delegation/proxy | +6–10 weeks |
| **Total to parity** | | **≈ 4–6 person-months**, realistically more under the verification regime |

The core slice alone makes DAVx5/Thunderbird/Apple/GNOME sync correctly; the
long tail is where sabre/dav earns its keep and where effort compounds.

## Options considered

- **A. Extend the vendored `dav-server` caldav modules.** Rejected as a
  foundation: file-based model, in-memory query scan, no RFC 6578, no write
  fan-out. Would be mostly deleted in the process of reaching parity.
- **B. Adopt RustiCal (dependency or wholesale vendor).** Rejected: store layer
  is unusable against the NC schema, ~60% of the NC surface is missing, and
  adopting inherits an orthogonal upstream roadmap.
- **C. Keep the status quo (delegate to PHP-FPM).** Works today; cal/card is
  not the hot path for most deployments (file sync is). Legitimate holding
  pattern, but keeps PHP-FPM load-bearing forever.
- **D. Build native when the time comes (recommended direction).** Calendar
  objects are DB blobs, so the `DavFileSystem` abstraction is *not* needed —
  native handlers over `nc-db` SQL in the style of
  `core-rs/crates/nc-dav/src/report.rs` and the existing `upload_handler`
  routing pattern are a better fit than forcing the tree through the vendored
  crate's filesystem trait. Harvest RustiCal's tested protocol logic (filters,
  sync-token math, ical parsing) as reference or vendor it selectively.
- **Incremental land-grab (recommended sequencing):** the router already
  supports per-subtree fallback granularity, so native can replace PHP subtree
  by subtree — core `calendars/`+`addressbooks/` read/sync traffic first, PHP
  fallback retained for scheduling, sharing mutations, resource booking, etc. —
  de-risking the port without a big-bang cutover.

## Recommendation

1. **Not now.** Phase 4 (files DAV tree) is still in progress and the
   delegation works. CalDAV/CardDAV should not jump the queue ahead of phases
   5–12.
2. **When scheduled:** option D with incremental land-grab; do *not* build on
   the vendored crate's caldav modules and do *not* depend on RustiCal.
   Budget ≈ 4–6 person-months to full parity, with the core slice
   (~6–10 weeks) as the first deliverable milestone.
3. **Before any implementation:** trace `CalDavBackend`/`CardDavBackend` write
   paths exhaustively (props index, changes, scheduling, reminders,
   invitations) per CLAUDE.md hygiene rules 3/6/7, and spec the subtrees under
   `SPECS/02-specifications/` in the usual file:line-grounded format.

---

Prev: [`16-differential-integration-test-harness.md`](16-differential-integration-test-harness.md) · Up: [`README.md`](README.md) · Next: (none)
