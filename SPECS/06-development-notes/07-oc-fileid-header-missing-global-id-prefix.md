# 07 — Rust's `OC-FileId` write header carried the bare numeric fileid, not PHP's global DAV id (zero-padded fileid + instance id), so the iOS client keyed the post-upload row differently from its SEARCH/PROPFIND rows

**Status:** diagnosed; fix not yet implemented. **Related:** [Phase 5.5 chunked uploads](../04-tasks/phase-5.md#55-chunked-upload-v2), [Phase 7.1 upload flows](../04-tasks/phase-7.1.md), [note 04](04-ios-photos-tab-upload-day-placement.md) (the same iOS Photos-tab client behavior).

## Observable failure

After every chunked media upload from the iOS client (Nextcloud-iOS 34.1.3), the **Media (Photos) tab** shows the just-uploaded file **twice**: one cell renders the preview (valid), the other is a broken/placeholder cell (invalid). The phantom cell only disappears when the user clears the client cache. The SEARCH response is server-side clean — each file appears exactly once with a valid `oc:fileid`, etag, and `has-preview`.

Live capture (mitmproxy, 2026-08-24/25) showed the full sequence on every upload:

1. chunked upload (PUTs + assembly MOVE to `Photos/YYYY/MM/`);
2. **two** `SEARCH /remote.php/dav` calls (~130 ms apart — the client's `searchMediaUI` and the `NCMediaMetadataBackfillProcessor`);
3. a depth-0 PROPFIND on the just-uploaded file (the client's per-file `readFile`).

The two SEARCH responses and the PROPFIND all returned the **same** `oc:id` for the file, e.g. `00408337ocecf7uk5jlr`, `oc:fileid` `408337`, same etag — yet the app kept two local rows for it.

## Root cause(s) — grounded

- **PHP's `OC-FileId` header is the *global* DAV file id.** `FilesPlugin::sendFileIdHeader` (`apps/dav/lib/Connector/Sabre/FilesPlugin.php:746-758`) runs on `afterBind`/`afterWriteContent` and calls `Node::getFileId()` → `DavUtil::getDavFileId($id)` (`lib/public/Files/DavUtil.php:26-30`): `sprintf('%08d', $id) . $instanceId`. So PHP returns `00408337ocecf7uk5jlr`, not `408337`.
- **Rust emits the bare numeric fileid** on both write paths:
  - chunked assembly MOVE: `upload_handler.rs:985` — `.header(HeaderName::from_static("oc-fileid"), …fid.to_string())`;
  - simple PUT: `handler.rs:744-748` — `parts.headers.insert(H_OC_FILEID, wr.fileid.to_string())`.
  Both are served natively by Rust (the iOS chunked upload MOVE is `upload_handler.rs`).
- **The iOS client keys its media rows on `OC-FileId`.** `uploadComplete` (`NCNetworking+NextcloudKitDelegate.swift:120-148`) reads `oc-fileid` from the response; the chunked upload path then calls `uploadSuccess` (`NCNetworking+Upload.swift:298-337`) which sets `metadata.ocId = ocId` (`:308`) and derives the numeric `fileId` via `NCUtility.ocIdToFileId` (`NCUtility.swift:17-24` — splits on `"oc"`, takes the numeric prefix).
- **The two sources therefore disagree on the row key.** The MOVE's `OC-FileId` yields `metadata.ocId = "408337"` (bare numeric), while the SEARCH response (PHP) yields `ocId = "00408337ocecf7uk5jlr"` (global id — the same value `createMetadata` gets from the per-file PROPFIND). `tableMetadata.primaryKey` is `ocId` (`NCManageDatabase+Metadata.swift:140`).
- **The sync never de-duplicates them.** `syncPlaceholderMetadatasAsync` (`NCManageDatabase+Metadata.swift:838+`) builds `filesByOcId` keyed by `ocId` and diffs against local metadatas by `ocId`; `"408337"` ≠ `"00408337…"`, so the SEARCH row is always "new" and the numeric-keyed placeholder row is never deleted. Clearing the client cache drops both, and the next SEARCH re-inserts the single global-id row — the observed "only goes away after clearing cache".

PHP serves SEARCH (proxied to PHP-FPM per `router.rs`) and Rust serves the upload MOVE natively, so the two `ocId`s diverge exactly at the Rust/PHP interop boundary.

## Options weighed

- **A. Change Rust to emit the global id.** Chosen. Both write paths already hold `instance_id` (`upload_handler.rs:955`, `handler.rs` via `NcDavState.instance_id`, `nc-dav/src/lib.rs:98`), and the bulk handler already formats exactly this way (`bulk_handler.rs:504-505`): `format!("{:08}{}", fid, instance_id)`. This restores byte-parity with PHP's header and makes the client's MOVE-time and SEARCH-time keys identical.
- **B. Leave the header bare and "fix" the client.** Rejected: the client is Nextcloud's own iOS app and reads `OC-FileId` per PHP's contract; and this rewrite's constitution is PHP parity at interop boundaries, not diverging and patching consumers.
- **C. Have Rust serve SEARCH natively too.** Out of scope: SEARCH is deliberately proxied to PHP (DASL backend); even if it were native it must still return PHP-compatible `oc:id`, so the header fix is required regardless.

## The choice

Emit `OC-FileId` as `format!("{:08}{}", fileid, instance_id)` on:
- `upload_handler.rs` chunked MOVE assembly response (header at `:985`);
- `handler.rs` simple-PUT `WriteResult` injection (`:744-748`) — reusing the existing `{oc:}id`-style formatting (already in `props.rs:102-103`) so the header, `{oc:}id` property, and SEARCH/PROPFIND `oc:id` all agree.

A regression test should assert the exact header string for both a chunked MOVE and a simple PUT against a fixed `instance_id`.

## Verification

- DB showed exactly one `oc_filecache` row per uploaded file (e.g. `408334`/`408337`), no duplicate rows, one `filecache_extended` row each — the server state is consistent; the duplication is purely the client's row keying on the divergent header.
- Live SEARCH (PHP) returned global `oc:id`s (`00408337…`) matching the depth-0 PROPFIND; only the Rust MOVE's `OC-FileId` differed (bare `408337`).
- Not yet verified: the header fix against the live phone / mitmproxy (expected: MOVE-time `ocId` == SEARCH-time `ocId`, so `replaceMetadataAsync`/`syncPlaceholderMetadatasAsync` collapse to one row), plus `cargo test` for the new regression.

## Follow-ups

1. Implement the header fix on both write paths + add regression tests; verify `cargo test -p nc-dav --lib`.
2. Re-capture with mitmproxy after a chunked upload from the iOS app; confirm a single SEARCH row and no phantom cell.
3. Grep the rest of the codebase for other `oc-fileid`/`OC-FileId` emitters (e.g. any remaining non-bulk path) to ensure the global-id format is universal.

Back: [`../README.md`](../README.md)
