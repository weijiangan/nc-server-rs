//! Handler for chunked upload v2 endpoints.
//!
//! Per PHASE-5.5: MKCOL /dav/uploads/{userId}/{upload_id} with Destination header required.
//! Per PHASE-5.6: PUT /dav/uploads/{userId}/{upload_id}/{part_id}
//! Per PHASE-5.7: MOVE /dav/uploads/{userId}/{upload_id}/.file
//! Per PHASE-5.8: DELETE /dav/uploads/{userId}/{upload_id}

use axum::{
    body::Body,
    extract::{Request, State},
    response::Response,
};
use http::{HeaderName, HeaderValue, StatusCode};
use nc_auth::AuthInfo;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::{row, NcDavState};

static H_CSP: HeaderName = HeaderName::from_static("content-security-policy");

/// Handler for chunked upload v2 endpoints.
///
/// Routes:
/// - MKCOL /dav/uploads/{userId}/{upload_id} - create upload slot
/// - PUT /dav/uploads/{userId}/{upload_id}/{part_id} - upload chunk
/// - MOVE /dav/uploads/{userId}/{upload_id}/.file - assemble chunks
/// - DELETE /dav/uploads/{userId}/{upload_id} - abort upload
pub async fn upload_handler(State(state): State<NcDavState>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Extract user ID from auth
    let uid = match req.extensions().get::<AuthInfo>() {
        Some(info) => info.uid.clone(),
        None => {
            return http::Response::builder()
                .status(401)
                .header("WWW-Authenticate", "Basic realm=\"Nextcloud\"")
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    };

    // Parse the upload path: /dav/uploads/{userId}/{upload_id}[/{part_id|.file}]
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // Expected: [dav, uploads, {userId}, {upload_id}]
    // or:       [remote.php, dav, uploads, {userId}, {upload_id}]
    // Find 'uploads' position
    let uploads_pos = path_parts.iter().position(|&s| s == "uploads");
    if uploads_pos.is_none() || path_parts.len() <= uploads_pos.unwrap() + 1 {
        return not_found_response();
    }
    let uploads_pos = uploads_pos.unwrap();

    // Verify userId matches authenticated user
    let path_user_id = path_parts.get(uploads_pos + 1).copied().unwrap_or("");
    if path_user_id != uid {
        return http::Response::builder()
            .status(403)
            .header(
                H_CSP.clone(),
                HeaderValue::from_static("default-src 'none';"),
            )
            .body(Body::from("Forbidden"))
            .unwrap();
    }

    let upload_id = path_parts.get(uploads_pos + 2).map(|s| s.to_string());

    match method.as_str() {
        "MKCOL" => handle_mkcol(state, req, upload_id.as_deref()).await,
        "PUT" => handle_put(state, req, upload_id.as_deref(), &path).await,
        "MOVE" => handle_move(state, req, &path, upload_id.as_deref()).await,
        "DELETE" => handle_delete(state, upload_id.as_deref(), &path).await,
        _ => method_not_allowed_response(),
    }
}

/// Handle MKCOL - create upload slot (PHASE-5.5)
async fn handle_mkcol(state: NcDavState, req: Request, upload_id: Option<&str>) -> Response {
    let upload_id = match upload_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            return bad_request_response("Missing upload_id");
        }
    };

    // Destination header is required per spec (PHASE-5.5)
    let destination = match req.headers().get("destination") {
        Some(h) => h.to_str().unwrap_or(""),
        None => return bad_request_response("Destination header required"),
    };

    // Parse the destination URL to extract target path
    let target_path = parse_destination_path(destination, &req.uri().path().to_string());

    // Parse OC-Total-Length header if present (PHASE-5.7)
    let expected_size: Option<u64> = req
        .headers()
        .get("oc-total-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // Store basic metadata in the in-process store
    state
        .upload_state_store
        .create_session(upload_id, target_path.clone(), None)
        .await;

    // Set expected size if provided
    if let Some(size) = expected_size {
        let _ = state
            .upload_state_store
            .set_expected_size(upload_id, size)
            .await;
    }

    tracing::debug!(
        upload_id = %upload_id,
        target = %target_path,
        expected_size = ?expected_size,
        "Created upload slot"
    );

    // Return 201 Created
    http::Response::builder()
        .status(StatusCode::CREATED)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::empty())
        .unwrap()
}

/// Parse the Destination header to extract the target path.
///
/// Handles:
/// - Full URL: http://host/dav/files/user/target.txt
/// - Absolute path: /dav/files/user/target.txt
fn parse_destination_path(destination: &str, _request_path: &str) -> String {
    // Strip scheme and host if present, extract the path component.
    let path = if let Some(rest) = destination.strip_prefix("http://") {
        rest.find('/').map(|p| &rest[p..]).unwrap_or(rest)
    } else if let Some(rest) = destination.strip_prefix("https://") {
        rest.find('/').map(|p| &rest[p..]).unwrap_or(rest)
    } else {
        destination
    };

    // Extract path relative to user's files. The destination should look like:
    //   /dav/files/{userId}/{target_path}
    // or:
    //   /remote.php/dav/files/{userId}/{target_path}
    //
    // Strategy: find "/files/" in the path and everything after the userId
    // segment becomes the target. Fall back to the raw path if parsing fails.
    if let Some(files_pos) = path.find("/files/") {
        let after_files = &path[files_pos + "/files/".len()..];
        // The first segment after /files/ is the userId; skip it.
        if let Some(slash) = after_files.find('/') {
            return after_files[slash..].to_string();
        }
        // Destination is user root (e.g., /dav/files/user)
        return String::new();
    }

    // Fallback: return the path as-is (stripped of leading /)
    path.trim_start_matches('/').to_string()
}

/// Handle PUT - upload chunk (PHASE-5.6)
async fn handle_put(
    state: NcDavState,
    req: Request,
    upload_id: Option<&str>,
    path: &str,
) -> Response {
    let upload_id = match upload_id {
        Some(id) => id,
        None => {
            return bad_request_response("Missing upload_id");
        }
    };

    // Check if session exists
    if !state.upload_state_store.session_exists(upload_id).await {
        return not_found_response();
    }

    // Parse part_id from path
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Find part_id: it's the last segment
    let part_id_str = path_parts.last().copied();

    // PHASE-5.6: Validate part_id is numeric and in range 1-10000
    let part_id: i64 = match part_id_str {
        Some(s) => match s.parse::<i64>() {
            Ok(n) => n,
            Err(_) => {
                return bad_request_response(
                    "Invalid chunk name, must be numeric between 1 and 10000",
                )
            }
        },
        None => return bad_request_response("Missing part_id"),
    };

    if !(part_id >= 1 && part_id <= 10000) {
        return bad_request_response("Invalid chunk name, must be numeric between 1 and 10000");
    }

    // Get Content-Length for the chunk
    let content_length: u64 = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    tracing::debug!(
        upload_id = %upload_id,
        part_id = part_id,
        size = content_length,
        "Uploading chunk"
    );

    // Get the user ID from the path
    let uploads_pos = path_parts.iter().position(|&s| s == "uploads").unwrap_or(0);
    let user_id = path_parts.get(uploads_pos + 1).copied().unwrap_or("");

    // ── §5.2 Quota enforcement (cumulative) ────────────────────────────────
    // PHP ChunkingV2Plugin::beforePut() checks quota against the cumulative
    // size of all uploaded chunks ($tempTargetFile->getSize() + $size),
    // not just the current chunk.  This prevents exceeding quota via many
    // small chunks that individually pass the check.
    if content_length > 0 {
        let data_dir_str = state.data_directory.to_str().unwrap_or("").to_string();
        if let Some(storage_id) =
            row::lookup_storage_id(&state.pool, &state.table_prefix, user_id, &data_dir_str).await
        {
            let previous_total = state
                .upload_state_store
                .get_total_chunk_size(upload_id)
                .await
                .unwrap_or(0);
            let cumulative_size = previous_total + content_length;

            if crate::quota::check_quota(
                &state.pool,
                &state.table_prefix,
                &state.appconfig_cache,
                user_id,
                storage_id,
                cumulative_size as i64,
            )
            .await
            .is_err()
            {
                return quota_exceeded_response();
            }
        }
    }

    // Create temp directory for chunks: {data_dir}/uploads/{uid}/{upload_id}/
    let chunk_dir = state
        .data_directory
        .join("uploads")
        .join(user_id)
        .join(upload_id);

    // Create the directory if it doesn't exist
    if let Err(e) = fs::create_dir_all(&chunk_dir).await {
        tracing::error!(error = %e, "Failed to create chunk directory");
        return internal_error_response();
    }

    // Write chunk to disk
    let chunk_path = chunk_dir.join(part_id.to_string());
    let mut file = match fs::File::create(&chunk_path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create chunk file");
            return internal_error_response();
        }
    };

    let body = req.into_body();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read request body");
            return internal_error_response();
        }
    };

    if let Err(e) = file.write_all(&bytes).await {
        tracing::error!(error = %e, "Failed to write chunk");
        let _ = fs::remove_file(&chunk_path).await;
        return internal_error_response();
    }

    if let Err(e) = file.flush().await {
        tracing::error!(error = %e, "Failed to flush chunk");
        return internal_error_response();
    }

    // Store chunk info in the upload state store
    if !state
        .upload_state_store
        .add_chunk(upload_id, part_id, content_length)
        .await
    {
        return not_found_response();
    }

    tracing::debug!(part_id = part_id, "Chunk stored at {:?}", chunk_path);

    // PHASE-5.6: Return 201 Created per spec
    http::Response::builder()
        .status(StatusCode::CREATED)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::empty())
        .unwrap()
}

/// Handle MOVE - assemble chunks (PHASE-5.7)
async fn handle_move(
    state: NcDavState,
    req: Request,
    path: &str,
    upload_id: Option<&str>,
) -> Response {
    let upload_id = match upload_id {
        Some(id) => id,
        None => {
            return bad_request_response("Missing upload_id");
        }
    };

    // Get upload metadata
    let _metadata = match state.upload_state_store.get_session(upload_id).await {
        Some(m) => m,
        None => return not_found_response(),
    };

    // Get Destination header (required for the final move)
    let destination = match req.headers().get("destination") {
        Some(d) => d.to_str().unwrap_or(""),
        None => return bad_request_response("Missing Destination header"),
    };

    // Parse the destination to get the target path
    let target_path = parse_destination_path(destination, path);

    // Validate the path is within the user's files directory
    if !target_path.starts_with('/') {
        return bad_request_response("Destination must be within /dav/files/");
    }

    // Parse OC-Total-Length header if present (PHASE-5.7)
    let oc_total_length: Option<u64> = req
        .headers()
        .get("oc-total-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // Validate sum of chunk sizes against OC-Total-Length if provided
    if let Some(expected_size) = oc_total_length {
        let actual_size = state
            .upload_state_store
            .get_total_chunk_size(upload_id)
            .await
            .unwrap_or(0);
        if actual_size != expected_size {
            let _ = cleanup_chunks(&state, upload_id, path).await;
            return bad_request_response(&format!(
                "OC-Total-Length mismatch: expected {}, got {}",
                expected_size, actual_size
            ));
        }
    }

    // Get sorted part IDs for assembly
    let part_ids = match state
        .upload_state_store
        .get_sorted_part_ids(upload_id)
        .await
    {
        Some(ids) if !ids.is_empty() => ids,
        _ => return bad_request_response("No chunks uploaded"),
    };

    // Get user ID from path
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let uploads_pos = path_parts.iter().position(|&s| s == "uploads").unwrap_or(0);
    let user_id = path_parts
        .get(uploads_pos + 1)
        .copied()
        .unwrap_or("")
        .to_string();

    tracing::debug!(
        upload_id = %upload_id,
        target_path = %target_path,
        part_count = part_ids.len(),
        "Assembling chunks"
    );

    // Read all chunks and concatenate them
    let chunk_dir = state
        .data_directory
        .join("uploads")
        .join(&user_id)
        .join(upload_id);

    let mut assembled_data = Vec::new();
    for part_id in &part_ids {
        let chunk_path = chunk_dir.join(part_id.to_string());
        match fs::read(&chunk_path).await {
            Ok(data) => assembled_data.extend(data),
            Err(e) => {
                tracing::error!(error = %e, part_id = part_id, "Failed to read chunk");
                return internal_error_response();
            }
        }
    }

    let total_size = assembled_data.len() as u64;

    // Look up storage ID for the user
    let storage_id = match row::lookup_storage_id(
        &state.pool,
        &state.table_prefix,
        &user_id,
        state.data_directory.to_str().unwrap_or(""),
    )
    .await
    {
        Some(id) => id,
        None => {
            tracing::error!(user_id = %user_id, "Failed to look up storage ID");
            return internal_error_response();
        }
    };

    // ── §5.2 Quota enforcement for assembly ────────────────────────────────
    // PHP ChunkingV2Plugin::beforeMove checks quota before assembly
    // (ChunkingV2Plugin.php lines 215-218).  Negative free_space() skips.
    if let Err(()) = crate::quota::check_quota(
        &state.pool,
        &state.table_prefix,
        &state.appconfig_cache,
        &user_id,
        storage_id,
        total_size as i64,
    )
    .await
    {
        let _ = cleanup_chunks(&state, upload_id, path).await;
        return quota_exceeded_response();
    }

    // Convert target path (e.g., /files/foo.txt) to filecache path (e.g., files/foo.txt)
    let fc_path = target_path.trim_start_matches('/');
    let dav_path = if fc_path.starts_with("files/") {
        fc_path.strip_prefix("files/").unwrap_or(fc_path)
    } else {
        fc_path
    };

    let fc_path_full = format!("files/{}", dav_path);

    // Check if target file already exists
    let existing_row =
        row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, &fc_path_full).await;
    let target_exists = existing_row.is_some();

    // Determine file name and extension
    let file_name = dav_path.rsplit('/').next().unwrap_or("").to_string();
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();

    // Resolve MIME type
    let mime_str = mime_guess::from_ext(&ext)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string();
    let part_str = mime_str
        .split('/')
        .next()
        .unwrap_or("application")
        .to_string();

    let (mime_type_id, mimepart_id) = {
        let cache = state.mime_cache.read().expect("mime cache lock");
        let mid = cache.get_id(&mime_str).unwrap_or(1);
        let pid = cache.get_id(&format!("{}/", part_str)).unwrap_or(1);
        (mid, pid)
    };

    // Resolve parent directory
    let parent_path = {
        let mut parts: Vec<&str> = dav_path.split('/').collect();
        parts.pop();
        if parts.is_empty() {
            "files".to_string()
        } else {
            format!("files/{}", parts.join("/"))
        }
    };

    let parent_row =
        match row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, &parent_path).await
        {
            Some(r) => r,
            None => {
                tracing::error!(parent_path = %parent_path, "Parent directory not found");
                return internal_error_response();
            }
        };

    // Create parent directories on disk if needed
    let parent_disk = row::disk_path(&state.data_directory, &user_id, &parent_path);
    if let Err(e) = fs::create_dir_all(&parent_disk).await {
        tracing::error!(error = %e, "Failed to create parent directory");
        return internal_error_response();
    }

    // Write the assembled file to disk
    let final_disk_path = row::disk_path(&state.data_directory, &user_id, &fc_path_full);

    if let Some(parent) = final_disk_path.parent() {
        if let Err(e) = fs::create_dir_all(parent).await {
            tracing::error!(error = %e, "Failed to create parent directory");
            return internal_error_response();
        }
    }

    if let Err(e) = fs::write(&final_disk_path, &assembled_data).await {
        tracing::error!(error = %e, "Failed to write assembled file");
        return internal_error_response();
    }

    // Get current time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Handle X-OC-MTime header (§10.5: validate with PHP MtimeSanitizer)
    let mtime = match crate::mtime::sanitize_mtime(
        req.headers().get("x-oc-mtime").and_then(|v| v.to_str().ok()),
    ) {
        Ok(Some(t)) => t,
        Ok(None) => now,
        Err(msg) => return bad_request_response(&msg),
    };

    // Handle X-OC-CTime header (§10.5: validate with PHP MtimeSanitizer)
    let ctime = match crate::mtime::sanitize_mtime(
        req.headers().get("x-oc-ctime").and_then(|v| v.to_str().ok()),
    ) {
        Ok(v) => v,
        Err(msg) => return bad_request_response(&msg),
    };

    // Set file mtime using filetime
    let t = filetime::FileTime::from_unix_time(mtime, 0);
    let _ = filetime::set_file_times(&final_disk_path, t, t);

    // Generate ETag (format: quoted 32-char hex UUID)
    let etag_raw = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let etag = etag_raw.clone();

    // Insert or update oc_filecache — allocate fileid for new files
    let fid: i64;
    if let Some(ref existing) = existing_row {
        fid = existing.fileid;
        let sql = format!(
            "UPDATE {prefix}filecache SET size=$1, mtime=$2, storage_mtime=$3, etag=$4, mimetype=$5, mimepart=$6 \
            WHERE fileid=$7",
            prefix = state.table_prefix
        );
        if let Err(e) = sqlx::query(&sql)
            .bind(total_size as i64)
            .bind(mtime)
            .bind(mtime)
            .bind(&etag)
            .bind(mime_type_id)
            .bind(mimepart_id)
            .bind(existing.fileid)
            .execute(&state.pool)
            .await
        {
            tracing::error!(error = %e, "Failed to update filecache");
        }
    } else {
        let hash = row::path_hash(&fc_path_full);
        let sql = format!(
            "INSERT INTO {prefix}filecache \
            (storage, path, path_hash, parent, name, mimetype, mimepart, \
             size, mtime, storage_mtime, etag, permissions, checksum) \
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
            RETURNING fileid",
            prefix = state.table_prefix
        );
        fid = match sqlx::query_scalar(&sql)
            .bind(storage_id)
            .bind(&fc_path_full)
            .bind(&hash)
            .bind(parent_row.fileid)
            .bind(&file_name)
            .bind(mime_type_id)
            .bind(mimepart_id)
            .bind(total_size as i64)
            .bind(mtime)
            .bind(mtime)
            .bind(&etag)
            .bind(27i32) // CRUDS permissions (READ|UPDATE|DELETE|SHARE)
            .bind("")
            .fetch_one(&state.pool)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "Failed to insert filecache");
                return internal_error_response();
            }
        };
    }

    // Update oc_filecache_extended with upload_time and optionally creation_time.
    // PHP storage layer sets upload_time=now for new files.
    // creation_time is set from X-OC-CTime header when provided.
    {
        let sql = format!(
            "INSERT INTO {prefix}filecache_extended (fileid, metadata_etag, creation_time, upload_time) \
            VALUES ($1, $2, $3, $4) \
            ON CONFLICT(fileid) DO UPDATE SET \
                creation_time = COALESCE(EXCLUDED.creation_time, {prefix}filecache_extended.creation_time), \
                upload_time = COALESCE(EXCLUDED.upload_time, {prefix}filecache_extended.upload_time), \
                metadata_etag = COALESCE(EXCLUDED.metadata_etag, {prefix}filecache_extended.metadata_etag)",
            prefix = state.table_prefix
        );
        let creation_time_val = ctime.unwrap_or(now);
        let upload_time_val = now; // always set upload_time for new/uploads
        let _ = sqlx::query(&sql)
            .bind(fid)
            .bind("") // metadata_etag
            .bind(creation_time_val)
            .bind(upload_time_val)
            .execute(&state.pool)
            .await;
    }

    // Clean up chunk directory
    if let Err(e) = fs::remove_dir_all(&chunk_dir).await {
        tracing::warn!(error = %e, "Failed to clean up chunk directory");
    }

    // Remove the upload session
    state.upload_state_store.remove_session(upload_id).await;

    // Build response
    let status = if target_exists {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };

    let mut builder = http::Response::builder()
        .status(status)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .header(
            HeaderName::from_static("oc-fileid"),
            existing_row
                .as_ref()
                .map(|r| r.fileid.to_string())
                .unwrap_or_else(|| fid.to_string()),
        )
        .header(HeaderName::from_static("etag"), format!("\"{}\"", &etag))
        .header(HeaderName::from_static("oc-etag"), format!("\"{}\"", &etag));

    if req.headers().get("x-oc-mtime").is_some() {
        builder = builder.header(HeaderName::from_static("x-oc-mtime"), "accepted");
    }
    if req.headers().get("x-oc-ctime").is_some() {
        builder = builder.header(HeaderName::from_static("x-oc-ctime"), "accepted");
    }

    builder.body(Body::empty()).unwrap()
}

/// Clean up chunk directory on error
async fn cleanup_chunks(state: &NcDavState, upload_id: &str, path: &str) {
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let uploads_pos = path_parts.iter().position(|&s| s == "uploads").unwrap_or(0);
    let user_id = path_parts.get(uploads_pos + 1).copied().unwrap_or("");

    let chunk_dir = state
        .data_directory
        .join("uploads")
        .join(user_id)
        .join(upload_id);

    let _ = fs::remove_dir_all(&chunk_dir).await;
}

/// Handle DELETE - abort upload (PHASE-5.8)
async fn handle_delete(state: NcDavState, upload_id: Option<&str>, path: &str) -> Response {
    let upload_id = match upload_id {
        Some(id) => id,
        None => {
            return bad_request_response("Missing upload_id");
        }
    };

    // Get user ID from path
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let uploads_pos = path_parts.iter().position(|&s| s == "uploads").unwrap_or(0);
    let user_id = path_parts.get(uploads_pos + 1).copied().unwrap_or("");

    // Remove the chunk directory on disk
    let chunk_dir = state
        .data_directory
        .join("uploads")
        .join(user_id)
        .join(upload_id);

    if chunk_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&chunk_dir).await {
            tracing::warn!(
                error = %e,
                upload_id = %upload_id,
                "Failed to remove chunk directory"
            );
        } else {
            tracing::debug!(upload_id = %upload_id, "Removed chunk directory");
        }
    }

    // Remove the upload session
    if state
        .upload_state_store
        .remove_session(upload_id)
        .await
        .is_some()
    {
        tracing::debug!(upload_id = %upload_id, "Aborted upload session");
    }

    // Return 204 No Content
    http::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::empty())
        .unwrap()
}

// ─── Response helpers ────────────────────────────────────────────────────────────

fn bad_request_response(msg: &str) -> Response {
    http::Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::from(msg.to_string()))
        .unwrap()
}

fn not_found_response() -> Response {
    http::Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::from("Not Found"))
        .unwrap()
}

fn method_not_allowed_response() -> Response {
    http::Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::from("Method Not Allowed"))
        .unwrap()
}

fn internal_error_response() -> Response {
    http::Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .body(Body::from("Internal server error"))
        .unwrap()
}

fn quota_exceeded_response() -> Response {
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  \
                <s:exception>OCA\\DAV\\Connector\\Sabre\\Exception\\InsufficientStorage</s:exception>\n  \
                <s:message>Quota exceeded: insufficient free space to upload.</s:message>\n\
                </d:error>\n";
    http::Response::builder()
        .status(StatusCode::INSUFFICIENT_STORAGE)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .header(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(body))
        .unwrap()
}

// ─── Tests ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_destination_path_absolute() {
        let result =
            parse_destination_path("/dav/files/user/test.txt", "/dav/uploads/user/upload123");
        assert_eq!(result, "/test.txt");
    }

    #[test]
    fn test_parse_destination_path_with_subdirectory() {
        let result = parse_destination_path(
            "/dav/files/user/folder/subfolder/file.txt",
            "/dav/uploads/user/upload123",
        );
        assert_eq!(result, "/folder/subfolder/file.txt");
    }

    #[test]
    fn test_parse_destination_path_http_url() {
        let result = parse_destination_path(
            "http://localhost/dav/files/user/test.txt",
            "/dav/uploads/user/upload123",
        );
        assert_eq!(result, "/test.txt");
    }

    #[test]
    fn test_parse_destination_path_https_url() {
        let result = parse_destination_path(
            "https://localhost/dav/files/user/test.txt",
            "/dav/uploads/user/upload123",
        );
        assert_eq!(result, "/test.txt");
    }

    #[test]
    fn test_parse_destination_path_remote_php_prefix() {
        let result = parse_destination_path(
            "/remote.php/dav/files/user/subdir/file.txt",
            "/remote.php/dav/uploads/user/upload123",
        );
        assert_eq!(result, "/subdir/file.txt");
    }

    #[test]
    fn test_parse_destination_path_no_dav_files_prefix() {
        let result = parse_destination_path("/other/path/file.txt", "/dav/uploads/user/upload123");
        assert_eq!(result, "other/path/file.txt");
    }

    #[test]
    fn test_bad_request_response_status() {
        let resp = bad_request_response("test error");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_not_found_response() {
        let resp = not_found_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_method_not_allowed_response() {
        let resp = method_not_allowed_response();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn test_parse_destination_path_root() {
        // Destination for root of user's files: /dav/files/user
        let result = parse_destination_path("/dav/files/user", "/dav/uploads/user/upload123");
        assert_eq!(result, "");
    }
}
