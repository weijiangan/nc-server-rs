# 01 — Static assets of apps installed outside `/apps` were unreachable in standalone mode

**Status:** fixed (commit `fe8f996`). **Related:** [Phase 18.1](../04-tasks/phase-18.md) (whitelist introduction), [Phase 18 Changes log](../04-tasks/phase-18.md) (2026-08-15 entry), [plan 19](../03-implementation-plan/plan/19-performance-improvements.md) (the perf work the whitelist was part of).

### What happened

The static-file serving check in `try_static_files_check` served only paths
under four **hardcoded** prefixes — `/core/ /dist/ /themes/ /apps/` — plus
`robots.txt` and `index.html` (Phase 18.1). On the live deployment every asset
of the memories app returned an empty axum 404 (0-byte body): the page HTML
loaded fine, but the browser then failed to fetch

```
/wapps/memories/js/memories-main.js?v=00426a1a-0   → 404, 0 bytes
```

— every asset, on every page load (captured in the HAR session
`cloud2.home.lan_wapps_memories_js_memories-main.js_Archive…har`,
2026-08-14).

### What we found out

1. **The web path is arbitrary config, not a webroot property.** PHP derives
   an app's web path from `config.php`'s `apps_paths` entries:
   `getAppWebPath()` returns `OC::$WEBROOT . $dir['url'] . '/' . $appId`
   (`AppManager.php:729`), where `$dir['url']` comes from `apps_paths`
   (`base.php:157-175`) and is rtrimmed of `/` (`base.php:161`). The live
   config has an entry `{path: /var/www/html/wapps, url: /wapps}` — verified
   by fetching the live page HTML with the captured session: it emits
   `src="/wapps/memories/js/memories-main.js"` exactly as `getAppWebPath`
   predicts. Nothing about `/wapps` is memories-specific — any app in any
   custom dir gets its assets behind an arbitrary url.
2. **The whitelist was "verified against the running webroot" — the dev
   webroot.** Phase 18.1's narrowing was checked against the dev SUT, whose
   webroot contains only core files. The dev config actually has **four**
   `apps_paths` urls (`/apps /apps-extra /apps-shared /apps-writable`) — the
   old whitelist was already wrong there too (verified: circles CSS under
   `/apps-extra/` was 404 before the fix, 200 after) — but nothing exercised
   app assets, and the probes only ever hit core paths.
3. **No other layer could serve the file.** The canonical reference
   deployment serves app assets from disk at the web-server layer (nginx
   `location ~ \.(css|js|…)$ { try_files $uri /index.php$request_uri; }` —
   extension-based, whole webroot). On live, nginx had no static locations
   and forwarded everything to nc-server; nc-server's registry covers only
   `/apps/*` and `/index.php/*`, and PHP's front controller has no route for
   raw JS. So the file was unreachable *by construction* in the
   Rust-fronted deployment — nginx didn't serve the tree, and neither Rust
   nor PHP could.

### Options considered

| Option | Mechanism | Cost / risk |
|---|---|---|
| **A. Extension-based matching** (nginx-faithful: `\.(css\|js\|svg\|…)$` + hide-rules) | Match any path ending in a static extension | **Adds an fs stat to every GET/HEAD with a static extension** — including DAV file GETs (`photo.jpg`, `song.flac`) — the hottest request class of this project. Exactly the per-request stat Phase 18.1 removed. Needs hide-rules (3rdparty/config/lib/dotfiles) to avoid serving repo files. |
| **B. Config-derived prefix allow-list** (chosen) | Fixed `/core/ /dist/ /themes/` + every `apps_paths` `url` → prefix; `/apps` fallback gated on the dir existing (PHP `base.php:167-170`) | Cost-neutral: same handful of `starts_with`, fs stat still only on candidates. Faithful: the same source PHP uses (`apps_paths`). |
| **C. Leave the whitelist as-is; serve statics from nginx on live** | Canonical static locations on the live nginx | Correct for live (that's the reference architecture), but standalone mode stays broken for any install with custom `apps_paths` — including the docker-standard `/custom_apps`. |

### What we chose and why

**Option B, plus C on live.** The whitelist is now derived once at startup by
`router::static_prefixes_from_config(cfg, nc_root)` (commit `fe8f996`):
`/core/ /dist/ /themes/` + every `apps_paths` url (rtrimmed, deduped) with
PHP's `/apps` default-root fallback when `apps_paths` is absent *and*
`<root>/apps` exists. Deliberate guards (documented in the code):

- a url that is empty or `/` after rtrim is **skipped** — as a prefix it would
  make every request path a static candidate (the Phase 18.1 stat regression);
  PHP tolerates webroot-root apps, but the canonical deployment serves those
  via nginx's extension regex without a stat;
- a url not starting with `/` is skipped (browser-relative — broken in PHP too);
- entries missing `url`/`path` are skipped (PHP's `isset` semantics);
- duplicates collapse.

Why not A: the per-request fs stat on DAV file GETs violates the project's
hot-path discipline for a surface (asset serving) that is not the
starvation thesis — and B achieves the same functional surface at zero cost.
Why C alone was insufficient: it fixes one deployment, not the standalone
mode the project ships (dev SUT, single-binary).

Live additionally got C (canonical static locations on the live nginx,
2026-08-15) — that is the reference architecture, and it makes Rust's static
layer dormant there.

### Verification

- Dev SUT (four `apps_paths`): `/apps-extra/circles/css/dashboard-*.css`
  404 → **200**; deny side unchanged (`/data/.ocdata` 404, `/apps-extra/` 404,
  `robots.txt` 200, `/core/img/actions/add.svg` 200).
- Live: `/wapps/memories/js/memories-main.js` → 200, 2.3 MB from disk with
  the canonical nginx headers (immutable cache-control, gzip).
- Tests: 7 derivation unit tests (`router.rs`), `static_serves_custom_app_dir`
  + `static_denies_unlisted_app_dir` integration pins (`auth.rs`),
  `apps_paths_parses` (`nc-db`); 50 nc-server + 39 nc-db green.

### Future improvements

1. **Full nginx-faithful standalone surface.** Option A stays on the table if
   standalone mode ever must serve arbitrary webroot files exactly like
   canonical nginx (extension regex + hide-rules + `try_files` fallback). The
   cost to revisit is the per-request fs stat — it could be mitigated with a
   small negative TTL cache keyed on path prefix.
2. **`config.php` hot-reload.** `apps_paths` (like other config) is read once
   at startup; changing it requires a restart. A config watcher (already
   listed as improvement I.1 in `02-specifications/improvements.md` for
   other keys) would cover it too.
3. **Fallback config parser.** `NcConfig::from_php_config` cannot parse
   nested arrays — a config with `apps_paths` requires the PHP-CLI loader
   (the primary path). If the PHP-CLI fallback ever needs to be
   self-sufficient, nested-array parsing is the gap. Pre-existing, unchanged.
4. **Diff-test coverage for app assets.** Nothing in the differential suite
   exercises static serving; the whitelist can drift again without a
   scenario. A scenario probing `/apps-extra/…` assets (or a probe list entry
   in the bench harness) would have caught this at the 18.1 stop.
