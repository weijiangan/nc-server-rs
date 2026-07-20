//! Native preview / thumbnail handlers (Phase 11.2 — "serve cache hits natively").
//!
//! Routes: `/core/preview` (by fileId), `/core/preview.png` (by path),
//! `/apps/files/api/v1/thumbnail/{x}/{y}/{file}` (deprecated, crop forced).
//!
//! ## Design: fast-path hits, proxy everything else
//!
//! A request is served natively **only** when the file is verified to be in the
//! caller's own home storage *and* a matching cached preview row exists. Every
//! other case is delegated to PHP-FPM, so behaviour is never worse than today and
//! there is **no IDOR surface**: a fileId/path that is not the caller's own
//! home-storage file is proxied to PHP, which performs the full user-folder-scoped
//! resolution (`getFirstNodeById` / `userFolder->get`, including share mounts) and
//! the share `hide_download`/`canSeeContent` authz. Shared-with-me previews thus
//! still go through PHP (correct), while the dominant gallery case — a user's own
//! photos — is served with zero PHP.
//!
//! Misses (no max preview, or the bucketed variant not yet generated) also proxy to
//! PHP until native generation lands (11.4); `forceIcon=false` proxies too, since it
//! engages PHP's `isAvailable` gate before serving.

use axum::{
    extract::{Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    Extension,
};
use nc_auth::AuthInfo;
use nc_dav::row::FileCacheRow;
use nc_preview::{
    response::{self, RouteKind},
    size::{self, Mode},
    store,
};
use std::time::SystemTime;

use crate::state::AppState;

/// `Constants::PERMISSION_READ`.
const PERMISSION_READ: i32 = 1;

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /core/preview?fileId=…` — preview by file id (`PreviewController::getPreviewByFileId`).
pub async fn preview_by_file_id(
    State(state): State<AppState>,
    auth: Option<Extension<AuthInfo>>,
    req: Request,
) -> Response {
    let q = query_map(req.uri());
    let file_id = q_i64(&q, "fileId", -1);
    let x = q_i64(&q, "x", 32);
    let y = q_i64(&q, "y", 32);
    if file_id == -1 || x == 0 || y == 0 {
        return bad_request(RouteKind::Core);
    }
    let a = q_bool(&q, "a", false);
    let mode = Mode::from_php(&q_str(&q, "mode", "fill"));
    let force_icon = q_bool(&q, "forceIcon", true);
    let inm = header_str(&req, header::IF_NONE_MATCH);
    let ims = header_str(&req, header::IF_MODIFIED_SINCE);

    let Some(uid) = auth.map(|e| e.0.uid) else {
        return proxy(&state, req).await;
    };
    // forceIcon=false engages PHP's isAvailable gate; let PHP handle that path.
    if !force_icon {
        return proxy(&state, req).await;
    }
    let Some(row) = resolve_by_id(&state, &uid, file_id).await else {
        // Not the caller's own home-storage file → PHP does the full resolution
        // (share mounts) and returns the correct 404/403.
        return proxy(&state, req).await;
    };
    serve_preview(&state, RouteKind::Core, row, x, y, !a, mode, inm, ims, req).await
}

/// `GET /core/preview.png?file=…` — preview by path (`PreviewController::getPreview`).
pub async fn preview_by_path(
    State(state): State<AppState>,
    auth: Option<Extension<AuthInfo>>,
    req: Request,
) -> Response {
    let q = query_map(req.uri());
    let file = q_str(&q, "file", "");
    let x = q_i64(&q, "x", 32);
    let y = q_i64(&q, "y", 32);
    if file.is_empty() || x == 0 || y == 0 {
        return bad_request(RouteKind::Core);
    }
    let a = q_bool(&q, "a", false);
    let mode = Mode::from_php(&q_str(&q, "mode", "fill"));
    let force_icon = q_bool(&q, "forceIcon", true);
    let inm = header_str(&req, header::IF_NONE_MATCH);
    let ims = header_str(&req, header::IF_MODIFIED_SINCE);

    let Some(uid) = auth.map(|e| e.0.uid) else {
        return proxy(&state, req).await;
    };
    if !force_icon {
        return proxy(&state, req).await;
    }
    let Some(row) = resolve_by_path(&state, &uid, &file).await else {
        return proxy(&state, req).await;
    };
    serve_preview(&state, RouteKind::Core, row, x, y, !a, mode, inm, ims, req).await
}

/// `GET /apps/files/api/v1/thumbnail/{x}/{y}/{file}` — deprecated files thumbnail
/// (`ApiController::getThumbnail`): crop forced true, JSON error bodies, no `cacheFor`.
pub async fn files_thumbnail(
    State(state): State<AppState>,
    auth: Option<Extension<AuthInfo>>,
    req: Request,
) -> Response {
    let Some((x, y, file)) = parse_thumbnail_path(req.uri()) else {
        return bad_request(RouteKind::FilesThumbnail);
    };
    if x < 1 || y < 1 {
        return bad_request(RouteKind::FilesThumbnail);
    }
    let inm = header_str(&req, header::IF_NONE_MATCH);
    let ims = header_str(&req, header::IF_MODIFIED_SINCE);

    let Some(uid) = auth.map(|e| e.0.uid) else {
        return proxy(&state, req).await;
    };
    let Some(row) = resolve_by_path(&state, &uid, &file).await else {
        // Not in the caller's home storage → PHP resolves shares / returns the
        // JSON 404 ("File not found.").
        return proxy(&state, req).await;
    };
    // crop=true always; mode defaults to fill (PHP passes no mode here).
    serve_preview(&state, RouteKind::FilesThumbnail, row, x, y, true, Mode::Fill, inm, ims, req)
        .await
}

// ─── Shared serve path ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn serve_preview(
    state: &AppState,
    kind: RouteKind,
    fc_row: FileCacheRow,
    x: i64,
    y: i64,
    crop: bool,
    mode: Mode,
    inm: Option<String>,
    ims: Option<String>,
    req: Request,
) -> Response {
    if !state.preview_registry.enable_previews() {
        return not_found(kind);
    }
    if fc_row.permissions & PERMISSION_READ == 0 {
        return forbidden(kind);
    }
    let file_id = fc_row.fileid;

    let rows = store::load_preview_rows(&state.pool, &state.table_prefix, file_id).await;
    let Some(max) = store::find_max(&rows, -1) else {
        // No max preview generated yet → PHP generates + serves.
        return proxy(state, req).await;
    };

    // Bucket the requested size relative to the max preview's actual dimensions.
    let (bw, bh) = size::calculate_size(x, y, crop, mode, max.width as i64, max.height as i64);
    let target = if bw == max.width && bh == max.height {
        max
    } else {
        match store::find_match(&rows, bw, bh, crop, max.mimetype_id, -1) {
            Some(r) => r,
            // Bucketed variant not generated yet → PHP derives it from the max.
            None => return proxy(state, req).await,
        }
    };

    // Snapshot the row's fields (owned) before any further await.
    let t_w = target.width;
    let t_h = target.height;
    let t_crop = target.cropped;
    let t_max = target.max;
    let t_version = target.version_id;
    let t_mime = target.mimetype_id;
    let t_etag = target.etag.clone();
    let t_mtime = target.mtime;
    let t_size = target.size;

    let output_mime = {
        let cache = state.mime_cache.read().expect("mime cache lock");
        cache.get_name(t_mime as i64).map(|s| s.to_string())
    };
    let Some(output_mime) = output_mime else {
        return proxy(state, req).await;
    };

    let name = store::preview_name(t_version, t_w, t_h, t_crop, t_max, &output_mime);
    let path = store::preview_byte_path(&data_dir(state), &state.instanceid, file_id, &name);

    let pr = response::build_preview_response(
        kind,
        &output_mime,
        &t_etag,
        t_mtime,
        &name,
        t_size,
        inm.as_deref(),
        ims.as_deref(),
        SystemTime::now(),
    );
    if pr.status == 304 {
        return build_304(pr);
    }

    match tokio::fs::File::open(&path).await {
        Ok(file) => stream_200(pr, file).await,
        // Row exists but bytes are missing (orphan/race) → PHP regenerates.
        Err(_) => proxy(state, req).await,
    }
}

// ─── Resolution (IDOR-safe: home storage only) ────────────────────────────────

async fn home_storage_id(state: &AppState, uid: &str) -> Option<i64> {
    let data_dir = data_dir(state);
    let data_dir_str = data_dir.to_string_lossy();
    nc_dav::row::lookup_storage_id(&state.pool, &state.table_prefix, uid, &data_dir_str).await
}

/// Resolve a fileId to the caller's **own** home-storage row, or `None` (which the
/// caller treats as "proxy to PHP" — covering shares and other users' files safely).
async fn resolve_by_id(state: &AppState, uid: &str, file_id: i64) -> Option<FileCacheRow> {
    let home_id = home_storage_id(state, uid).await?;
    let row = nc_dav::row::lookup_by_id(&state.pool, &state.table_prefix, file_id).await?;
    (row.storage == home_id).then_some(row)
}

/// Resolve a user-relative path within the caller's home storage, or `None`.
async fn resolve_by_path(state: &AppState, uid: &str, path: &str) -> Option<FileCacheRow> {
    let home_id = home_storage_id(state, uid).await?;
    let fc_path = nc_dav::row::dav_to_fc_path(path);
    nc_dav::row::lookup_by_path(&state.pool, &state.table_prefix, home_id, &fc_path).await
}

fn data_dir(state: &AppState) -> std::path::PathBuf {
    state
        .nc_config
        .datadirectory
        .clone()
        .unwrap_or_else(|| state.nc_root.join("data"))
}

// ─── Response builders ────────────────────────────────────────────────────────

fn header_val(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn stream_200(pr: response::PreviewResponse, file: tokio::fs::File) -> Response {
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, header_val(&pr.content_type))
        .header(header::CONTENT_LENGTH, pr.content_length)
        .header(header::ETAG, header_val(&pr.etag))
        .header(header::LAST_MODIFIED, header_val(&pr.last_modified))
        .header(header::CACHE_CONTROL, header_val(&pr.cache_control))
        .header(header::CONTENT_DISPOSITION, header_val(&pr.content_disposition))
        .header("X-Robots-Tag", response::X_ROBOTS_TAG);
    if let Some(ref exp) = pr.expires {
        b = b.header(header::EXPIRES, header_val(exp));
    }
    b.body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn build_304(pr: response::PreviewResponse) -> Response {
    let mut b = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, header_val(&pr.etag))
        .header(header::LAST_MODIFIED, header_val(&pr.last_modified))
        .header(header::CACHE_CONTROL, header_val(&pr.cache_control))
        .header("X-Robots-Tag", response::X_ROBOTS_TAG);
    if let Some(ref exp) = pr.expires {
        b = b.header(header::EXPIRES, header_val(exp));
    }
    b.body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn not_found(kind: RouteKind) -> Response {
    match kind {
        RouteKind::Core => StatusCode::NOT_FOUND.into_response(),
        RouteKind::FilesThumbnail => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"message": "File not found."})),
        )
            .into_response(),
    }
}

fn forbidden(kind: RouteKind) -> Response {
    match kind {
        // Core maps NotPermitted → 403; files maps it → 404 JSON.
        RouteKind::Core => StatusCode::FORBIDDEN.into_response(),
        RouteKind::FilesThumbnail => not_found(kind),
    }
}

fn bad_request(kind: RouteKind) -> Response {
    match kind {
        RouteKind::Core => StatusCode::BAD_REQUEST.into_response(),
        RouteKind::FilesThumbnail => (
            StatusCode::BAD_REQUEST,
            axum::Json(
                serde_json::json!({"message": "Requested size must be numeric and a positive value."}),
            ),
        )
            .into_response(),
    }
}

async fn proxy(state: &AppState, req: Request) -> Response {
    match state.fastcgi.as_ref() {
        Some(fpm) => nc_fastcgi::proxy_handler(fpm, req).await,
        None => StatusCode::BAD_GATEWAY.into_response(),
    }
}

// ─── Param parsing ────────────────────────────────────────────────────────────

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn query_map(uri: &Uri) -> std::collections::HashMap<String, String> {
    let Some(q) = uri.query() else {
        return std::collections::HashMap::new();
    };
    q.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k.is_empty() {
                None
            } else {
                Some((decode(k), decode(v)))
            }
        })
        .collect()
}

fn q_i64(q: &std::collections::HashMap<String, String>, key: &str, default: i64) -> i64 {
    q.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn q_bool(q: &std::collections::HashMap<String, String>, key: &str, default: bool) -> bool {
    q.get(key)
        .map(|v| matches!(v.as_str(), "true" | "1"))
        .unwrap_or(default)
}

fn q_str(q: &std::collections::HashMap<String, String>, key: &str, default: &str) -> String {
    q.get(key).cloned().unwrap_or_else(|| default.to_string())
}

fn header_str(req: &Request, name: HeaderName) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Parse `/apps/files/api/v1/thumbnail/{x}/{y}/{file+}` → `(x, y, file)`.
/// Non-numeric `x`/`y` parse to `0` (the handler then rejects with the JSON 400,
/// matching PHP's cast-to-int → `< 1`).
fn parse_thumbnail_path(uri: &Uri) -> Option<(i64, i64, String)> {
    let rest = uri.path().strip_prefix("/apps/files/api/v1/thumbnail/")?;
    let mut parts = rest.splitn(3, '/');
    let x = parts.next()?.parse::<i64>().unwrap_or(0);
    let y = parts.next()?.parse::<i64>().unwrap_or(0);
    let file = decode(parts.next()?);
    Some((x, y, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn query_map_decodes_and_defaults() {
        let q = query_map(&uri("/core/preview?fileId=42&x=100&y=200&a=true&mode=cover"));
        assert_eq!(q_i64(&q, "fileId", -1), 42);
        assert_eq!(q_i64(&q, "x", 32), 100);
        assert_eq!(q_i64(&q, "y", 32), 200);
        assert!(q_bool(&q, "a", false));
        assert_eq!(q_str(&q, "mode", "fill"), "cover");
        // defaults when absent
        let q2 = query_map(&uri("/core/preview"));
        assert_eq!(q_i64(&q2, "fileId", -1), -1);
        assert_eq!(q_i64(&q2, "x", 32), 32);
        assert!(!q_bool(&q2, "a", false));
        assert!(q_bool(&q2, "forceIcon", true));
    }

    #[test]
    fn query_map_url_decodes_file() {
        let q = query_map(&uri("/core/preview.png?file=Photos%2Fmy%20img.jpg"));
        assert_eq!(q_str(&q, "file", ""), "Photos/my img.jpg");
    }

    #[test]
    fn thumbnail_path_parses() {
        let (x, y, f) =
            parse_thumbnail_path(&uri("/apps/files/api/v1/thumbnail/256/512/Photos/img.jpg"))
                .unwrap();
        assert_eq!((x, y), (256, 512));
        assert_eq!(f, "Photos/img.jpg");
        // nested path + decoding
        let (_, _, f2) =
            parse_thumbnail_path(&uri("/apps/files/api/v1/thumbnail/64/64/a%2Fb%20c.png")).unwrap();
        assert_eq!(f2, "a/b c.png");
    }

    #[test]
    fn thumbnail_path_non_numeric_is_zero() {
        let (x, y, _) =
            parse_thumbnail_path(&uri("/apps/files/api/v1/thumbnail/abc/64/x.png")).unwrap();
        assert_eq!(x, 0); // handler rejects x<1 → JSON 400 (PHP parity)
        assert_eq!(y, 64);
    }

    /// The native preview routes must coexist with the PHP-FPM wildcard routes the
    /// registry adds (`/apps/files/{*tail}`, and `/core/{*tail}` if `/core` is a
    /// registry base).  Building the router panics on an ambiguous/duplicate route,
    /// so this test guards the registration in `router::build`.
    #[test]
    fn native_routes_coexist_with_php_wildcards() {
        use axum::{routing::get, Router};
        async fn h() {}
        // Native routes are registered first (static block), then the registry fold
        // adds the wildcards — mirror that order here.
        let _r: Router = Router::new()
            .route("/core/preview", get(h))
            .route("/core/preview.png", get(h))
            .route("/apps/files/api/v1/thumbnail/{x}/{y}/{*file}", get(h))
            .route("/core/{*tail}", get(h))
            .route("/core", get(h))
            .route("/apps/files/{*tail}", get(h))
            .route("/apps/files", get(h));
    }
}
