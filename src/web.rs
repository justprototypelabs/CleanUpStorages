//! Local, read-only web browse/search UI. Binds 127.0.0.1 only.

use crate::catalog::models::FileRecord;
use crate::catalog::store::SearchFilters;
use crate::catalog::Catalog;
use axum::extract::Path as AxPath;
use axum::http::HeaderMap;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{
    extract::Query,
    extract::State,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;

#[derive(Clone)]
pub struct AppState {
    pub catalog_path: PathBuf,
    pub mounts: crate::mounts::MountResolver,
    pub csrf_token: String,
    pub scan_queue: std::sync::Arc<crate::scan_queue::ScanQueue>,
    /// Serial worker for folder and single-file quarantines, so confirming one does not block
    /// the next (#66).
    pub quarantine_queue: std::sync::Arc<crate::quarantine_queue::QuarantineQueue>,
}

impl AppState {
    /// Production state: live mount detection and a fresh random CSRF token.
    pub fn new_live(catalog_path: PathBuf) -> AppState {
        AppState {
            mounts: crate::mounts::MountResolver::Live {
                catalog_path: catalog_path.clone(),
            },
            csrf_token: uuid::Uuid::new_v4().to_string(),
            scan_queue: crate::scan_queue::ScanQueue::new(catalog_path.clone()),
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                catalog_path.clone(),
                crate::mounts::MountResolver::Live {
                    catalog_path: catalog_path.clone(),
                },
            ),
            catalog_path,
        }
    }
}

/// Convenience builder used by the CLI and existing tests (live mounts, random token).
pub fn build_router(catalog_path: PathBuf) -> Router {
    build_router_with(AppState::new_live(catalog_path))
}

/// The full router. New review routes are added here in later tasks.
pub fn build_router_with(state: AppState) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/browse", get(browse))
        .route("/api/search", get(api_search))
        .route("/api/status-counts", get(api_status_counts))
        .route("/api/volumes", get(api_volumes))
        .route("/api/folder", get(api_folder))
        .route("/api/stats", get(api_stats))
        .route("/api/activity", get(api_activity))
        .route("/api/drives", get(api_drives))
        .route("/api/detected-drives", get(api_detected_drives))
        .route("/api/duplicates", get(api_duplicates))
        .route("/api/tree-duplicates", get(api_tree_duplicates))
        .route("/api/quarantine-tree", post(api_quarantine_tree))
        .route("/api/quarantine/status", get(api_quarantine_status))
        .route("/api/copies", get(api_copies))
        .route("/api/volumes/:id/errors", get(api_volume_errors))
        .route("/api/preview/:id", get(api_preview))
        .route("/api/quarantine", post(api_quarantine))
        .route("/api/repack", post(api_repack))
        .route("/api/forget-drive", post(api_forget_drive))
        .route("/api/rename-drive", post(api_rename_drive))
        .route("/api/purge-all", post(api_purge_all))
        .route("/api/scan", post(api_scan))
        .route("/api/scan/status", get(api_scan_status))
        .route("/api/scan/stop", post(api_scan_stop))
        .route("/api/scan-runs", get(api_scan_runs))
        .route("/api/pick-folder", post(api_pick_folder))
        .route(
            "/api/settings",
            get(api_settings_get).post(api_settings_post),
        )
        .route("/api/pending-formats", get(api_pending_formats))
        .route("/api/pending-formats/resolve", post(api_resolve_format))
        .route("/review", get(review))
        .route("/scan", get(scan_page_h))
        .route("/drives", get(drives_page_h))
        .route("/console", get(console_page_h))
        .route("/assets/:file", get(asset))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<axum::body::Body>| {
                    crate::observability::make_request_span(req)
                })
                .on_request(DefaultOnRequest::new().level(tracing::Level::DEBUG))
                .on_response(
                    DefaultOnResponse::new()
                        .level(tracing::Level::INFO)
                        .latency_unit(LatencyUnit::Millis),
                ),
        )
        .with_state(state)
}

async fn overview(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::overview_page(&state.csrf_token))
}

async fn browse(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::browse_page(&state.csrf_token))
}

async fn review(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::review_page(&state.csrf_token))
}

async fn scan_page_h(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::scan_page(&state.csrf_token))
}

async fn drives_page_h(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::drives_page(&state.csrf_token))
}

async fn console_page_h(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::console_page(&state.csrf_token))
}

/// Vendored fonts, served same-origin (no external request) and cached hard. Everything is
/// self-hosted so the UI stays 100% offline.
async fn asset(AxPath(file): AxPath<String>) -> Response {
    let bytes: &'static [u8] = match file.as_str() {
        "InterVariable.woff2" => include_bytes!("../assets/InterVariable.woff2"),
        "JetBrainsMono-Regular.woff2" => include_bytes!("../assets/JetBrainsMono-Regular.woff2"),
        "JetBrainsMono-Medium.woff2" => include_bytes!("../assets/JetBrainsMono-Medium.woff2"),
        "MaterialSymbolsOutlined.woff2" => {
            include_bytes!("../assets/MaterialSymbolsOutlined.woff2")
        }
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        axum::body::Bytes::from_static(bytes),
    )
        .into_response()
}

/// Web-facing shape for a search hit; keeps serialization concerns out of `catalog::models`.
#[derive(Serialize)]
struct HitDto {
    location: String,
    relative_path: String,
    /// Last valid on-disk location before quarantine (set only on quarantined/purged rows); the tree
    /// places these here instead of under `_ToDelete`.
    original_path: Option<String>,
    container_chain: Option<String>,
    filename: String,
    volume_id: String,
    volume_label: String,
    category: String,
    size_bytes: i64,
    status: String,
    content_hash: String,
    copies: Option<i64>,
}

impl From<FileRecord> for HitDto {
    fn from(f: FileRecord) -> HitDto {
        let location = f.display_location();
        HitDto {
            location,
            relative_path: f.relative_path,
            original_path: f.original_path,
            container_chain: f.container_chain,
            filename: f.filename,
            volume_id: f.volume_id,
            volume_label: String::new(), // filled by the handler (needs the catalog's label map)
            category: f.category.as_str().to_string(),
            size_bytes: f.size_bytes,
            status: f.status.as_str().to_string(),
            content_hash: f.content_hash,
            copies: None, // filled by the handler (needs the global duplicate counts)
        }
    }
}

#[derive(Serialize)]
struct DetectedDriveDto {
    mount_path: String,
    volume_id: Option<String>,
    catalogued: bool,
    volume_label: Option<String>,
    total_bytes: Option<u64>,
    free_bytes: Option<u64>,
}

#[derive(Serialize)]
struct VolumeDto {
    volume_id: String,
    label: String,
    display_name: Option<String>,
    active_files: i64,
    active_bytes: i64,
}

#[derive(Serialize)]
struct StatsDto {
    duplicate_groups: i64,
    /// Loose-only — what quarantine can actually reclaim by renaming.
    reclaimable_bytes: i64,
    /// Duplicated bytes inside archives. Reported separately; needs a repack, not a quarantine.
    archive_locked_bytes: i64,
    volumes: Vec<VolumeDto>,
}

#[derive(Serialize)]
struct ActivityDto {
    kind: String,
    summary: String,
    occurred_at: i64,
}

#[derive(Serialize)]
struct DriveDto {
    volume_id: String,
    label: String,
    display_name: Option<String>,
    description: Option<String>,
    mount_path: Option<String>, // None if not currently connected
    connected: bool,
    active_files: i64,
    active_bytes: i64,
    total_bytes: Option<u64>, // None if unmounted or undeterminable
    free_bytes: Option<u64>,
    reclaimable_bytes: i64, // potential: size of active duplicate copies not yet quarantined
    quarantined_bytes: i64, // actual: bytes sitting in `_ToDelete`, i.e. what "Purge" deletes
    last_seen_at: Option<i64>,
    has_errors: bool,
    absent: i64,
    unverified: i64,
    unreadable_dirs: i64,
}

/// Human summary for one audit row. `details` is the JSON stored by the engine; parse best-effort
/// and fall back to the raw action name so a schema change can never break the feed.
fn activity_summary(action: &str, details: &str) -> String {
    let d: serde_json::Value = serde_json::from_str(details).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |k: &str| d.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    match action {
        "scan" => format!(
            "Scanned {} — {} hashed, {} unchanged",
            s("label"),
            n("hashed"),
            n("skipped")
        ),
        "quarantine" => {
            let from = s("from");
            let name = from.rsplit('/').next().unwrap_or(&from);
            format!("Quarantined {name}")
        }
        "quarantine_skip" => "Skipped a file to protect the last copy".to_string(),
        "quarantine_error" => "A file could not be quarantined".to_string(),
        "repack" => format!("Repacked an archive (removed {})", s("removed_entry")),
        "purge" => format!(
            "Purged {} file(s), reclaimed {} MiB",
            n("files_purged"),
            n("bytes_reclaimed") / (1024 * 1024)
        ),
        "forget" => format!("Removed drive '{}' from the catalog", s("label")),
        "rename" => "Renamed a drive".to_string(),
        other => other.to_string(),
    }
}

#[derive(Deserialize, Default)]
struct SearchParams {
    q: Option<String>,
    category: Option<String>,
    volume: Option<String>,
    status: Option<String>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    modified_after: Option<i64>,
    modified_before: Option<i64>,
    limit: Option<usize>,
}

/// Map any error to a 500 with a short text body (localhost dev tool — plain messages are fine).
fn err500<E: std::fmt::Display>(e: E) -> (axum::http::StatusCode, String) {
    tracing::error!(error = %e, "request failed");
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// CSRF gate for mutating endpoints: require the per-run token (a cross-site page can't read it).
/// Call this FIRST in every mutating handler, before any catalog/filesystem/dialog access.
fn check_csrf(headers: &HeaderMap, state: &AppState) -> Result<(), (StatusCode, String)> {
    let ok = headers.get("x-cleanup-token").and_then(|v| v.to_str().ok())
        == Some(state.csrf_token.as_str());
    if !ok {
        tracing::warn!("rejected request: missing or bad CSRF token");
        return Err((StatusCode::FORBIDDEN, "missing or bad token".into()));
    }
    Ok(())
}

/// Current time as UNIX seconds; a clock error becomes a 500 (matches existing handler behavior).
fn now_secs() -> Result<i64, (StatusCode, String)> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(err500)?
        .as_secs() as i64)
}

/// Best-effort catalog snapshot around a mutation (some handlers call this before the mutation as a
/// pre-mutation safety net, others after). Never fails the request — a snapshot error is swallowed.
fn snapshot_best_effort(state: &AppState, now: i64) {
    // The snapshot goes beside the catalogue THIS request is mutating, derived from
    // `state.catalog_path` -- never from the ambient configuration.
    //
    // Resolving `Config::default_paths()` here was a real defect (#44): it sent the snapshot to
    // whichever catalogue the environment happened to point at, so `cargo test` wrote into the
    // user's genuine backups folder and retention then evicted the real pre-migration snapshots.
    // It was also simply wrong outside tests -- snapshotting catalogue A into catalogue B's backup
    // directory. Retention is a compile-time constant, so nothing here needs the config at all.
    let _ = crate::catalog::backup::snapshot_beside(&state.catalog_path, now);
}

/// Split a comma-separated query param (e.g. `status=active,quarantined`) into a filter vec,
/// dropping blanks. Multi-select filters send their chosen values this way.
fn csv(v: Option<String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

async fn api_search(
    State(state): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Result<Json<Vec<HitDto>>, (axum::http::StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let filters = SearchFilters {
        query: p.q.unwrap_or_default(),
        category: csv(p.category),
        volume: csv(p.volume),
        status: csv(p.status),
        min_size: p.min_size,
        max_size: p.max_size,
        modified_after: p.modified_after,
        modified_before: p.modified_before,
    };
    let limit = p.limit.unwrap_or(500).min(5000);
    let hits = cat.search_filtered(&filters, limit).map_err(err500)?;
    // Friendly drive names (effective: custom-or-detected) + which results are duplicated
    // (global active-copy count).
    let labels = cat.effective_labels().map_err(err500)?;
    let hashes: Vec<String> = hits.iter().map(|f| f.content_hash.clone()).collect();
    let dupes = cat.duplicate_counts(&hashes).map_err(err500)?;
    let out: Vec<HitDto> = hits
        .into_iter()
        .map(|f| {
            let mut dto = HitDto::from(f);
            dto.volume_label = labels
                .get(&dto.volume_id)
                .cloned()
                .unwrap_or_else(|| dto.volume_id.clone());
            dto.copies = dupes.get(&dto.content_hash).copied();
            dto
        })
        .collect();
    Ok(Json(out))
}

/// Count of catalogued rows per status for the current text/category/volume context, so the Browse
/// status filter can flag which kinds are present (including purged rows the tree hides by default).
async fn api_status_counts(
    State(state): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Result<Json<std::collections::HashMap<String, i64>>, (axum::http::StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let counts = cat
        .status_counts(&p.q.unwrap_or_default(), &csv(p.category), &csv(p.volume))
        .map_err(err500)?;
    Ok(Json(counts))
}

/// Shared by /api/volumes and /api/stats so the two endpoints can't drift apart.
fn volume_dtos(cat: &Catalog) -> anyhow::Result<Vec<VolumeDto>> {
    let eff = cat.effective_labels()?;
    Ok(cat
        .volume_stats()?
        .into_iter()
        .map(|(volume_id, label, active_files, active_bytes)| {
            let display_name = eff.get(&volume_id).cloned();
            VolumeDto {
                volume_id,
                label,
                display_name,
                active_files,
                active_bytes,
            }
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct FolderParams {
    volume: String,
    #[serde(default)]
    path: String,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(serde::Serialize)]
struct FolderDirDto {
    name: String,
    path: String,
    file_count: i64,
    total_bytes: i64,
}

#[derive(serde::Serialize)]
struct FolderDto {
    dirs: Vec<FolderDirDto>,
    files: Vec<HitDto>,
    /// True when the page filled up, so the caller knows to offer "load more" rather than guess.
    more_dirs: bool,
    more_files: bool,
}

/// One level of one drive's folder tree.
///
/// Browse used to build its whole tree client-side from a fixed slice of a path-ordered search.
/// That made every folder size a partial sum of the loaded rows and dropped any drive with nothing
/// in the slice -- on the real catalogue, the 4.75 TB drive vanished entirely because 39,171 rows
/// from the other drive sorted ahead of its first path. Serving one level at a time from
/// `directory_trees` means sizes are the folder's own and every drive is always present.
async fn api_folder(
    State(state): State<AppState>,
    Query(p): Query<FolderParams>,
) -> Result<Json<FolderDto>, (axum::http::StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let limit = p.limit.unwrap_or(200).clamp(1, 1000);
    let offset = p.offset.unwrap_or(0);

    // Ask for one more than requested: if it comes back, there is another page.
    let mut dirs = cat
        .folder_children(&p.volume, &p.path, limit + 1, offset)
        .map_err(err500)?;
    let more_dirs = dirs.len() > limit;
    dirs.truncate(limit);

    let mut files = cat
        .folder_files(&p.volume, &p.path, limit + 1, offset)
        .map_err(err500)?;
    let more_files = files.len() > limit;
    files.truncate(limit);

    let labels = cat.effective_labels().map_err(err500)?;
    let hashes: Vec<String> = files.iter().map(|f| f.content_hash.clone()).collect();
    let dupes = cat.duplicate_counts(&hashes).map_err(err500)?;
    let files = files
        .into_iter()
        .map(|f| {
            let mut dto = HitDto::from(f);
            dto.volume_label = labels
                .get(&dto.volume_id)
                .cloned()
                .unwrap_or_else(|| dto.volume_id.clone());
            dto.copies = dupes.get(&dto.content_hash).copied();
            dto
        })
        .collect();

    Ok(Json(FolderDto {
        dirs: dirs
            .into_iter()
            .map(|d| FolderDirDto {
                name: d.name,
                path: d.path,
                file_count: d.file_count,
                total_bytes: d.total_bytes,
            })
            .collect(),
        files,
        more_dirs,
        more_files,
    }))
}

async fn api_volumes(
    State(state): State<AppState>,
) -> Result<Json<Vec<VolumeDto>>, (axum::http::StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    Ok(Json(volume_dtos(&cat).map_err(err500)?))
}

async fn api_stats(
    State(state): State<AppState>,
) -> Result<Json<StatsDto>, (axum::http::StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    // Floor-free: the Overview headline must never move because of a review-list filter.
    let totals = cat
        .duplicate_totals(crate::catalog::dedup::DEFAULT_MIN_SIZE)
        .map_err(err500)?;
    let volumes = volume_dtos(&cat).map_err(err500)?;
    Ok(Json(StatsDto {
        duplicate_groups: totals.groups_all,
        reclaimable_bytes: totals.reclaimable_all_bytes,
        archive_locked_bytes: totals.archive_locked_bytes,
        volumes,
    }))
}

async fn api_activity(
    State(state): State<AppState>,
) -> Result<Json<Vec<ActivityDto>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let rows = cat.recent_actions(30).map_err(err500)?;
    Ok(Json(
        rows.into_iter()
            .map(|(action, details, occurred_at)| ActivityDto {
                summary: activity_summary(&action, &details),
                kind: action,
                occurred_at,
            })
            .collect(),
    ))
}

async fn api_drives(
    State(state): State<AppState>,
) -> Result<Json<Vec<DriveDto>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let reclaim = cat.reclaimable_by_volume().map_err(err500)?;
    let mounts = state.mounts.snapshot();
    let mut out = Vec::new();
    for (volume_id, label, active_files, active_bytes) in cat.volume_stats().map_err(err500)? {
        let mount_path = mounts.get(&volume_id).cloned();
        let (total_bytes, free_bytes) = match &mount_path {
            Some(p) => match crate::mounts::disk_capacity(p) {
                Some((t, f)) => (Some(t), Some(f)),
                None => (None, None),
            },
            None => (None, None),
        };
        let (display_name, description) = cat.volume_meta(&volume_id).map_err(err500)?;
        let completeness = cat.volume_completeness(&volume_id).map_err(err500)?;
        // Derived, so the pill stops latching: self-heal removes the rows, the count drops to
        // zero, and the drive stops claiming an error it no longer has.
        let has_errors = !completeness.is_complete();
        out.push(DriveDto {
            connected: mount_path.is_some(),
            mount_path: mount_path.map(|p| p.display().to_string()),
            reclaimable_bytes: reclaim.get(&volume_id).copied().unwrap_or(0),
            quarantined_bytes: cat.recoverable_bytes(&volume_id).map_err(err500)?,
            last_seen_at: cat.volume_last_seen(&volume_id).map_err(err500)?,
            has_errors,
            absent: completeness.absent,
            unverified: completeness.unverified,
            unreadable_dirs: completeness.unreadable_dirs,
            volume_id,
            label,
            display_name,
            description,
            active_files,
            active_bytes,
            total_bytes,
            free_bytes,
        });
    }
    Ok(Json(out))
}

async fn api_detected_drives(
    State(state): State<AppState>,
) -> Result<Json<Vec<DetectedDriveDto>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let labels: std::collections::HashMap<String, String> = cat
        .volume_stats()
        .map_err(err500)?
        .into_iter()
        .map(|(id, label, _, _)| (id, label))
        .collect();
    let mut out = Vec::new();
    for (_vid_key, root) in state.mounts.snapshot() {
        let volume_id = crate::volume::read_volume_id(&root);
        let (catalogued, volume_label) = match &volume_id {
            Some(vid) => (labels.contains_key(vid), labels.get(vid).cloned()),
            None => (false, None),
        };
        let (total_bytes, free_bytes) = match crate::mounts::disk_capacity(&root) {
            Some((t, f)) => (Some(t), Some(f)),
            None => (None, None),
        };
        out.push(DetectedDriveDto {
            mount_path: root.display().to_string(),
            volume_id,
            catalogued,
            volume_label,
            total_bytes,
            free_bytes,
        });
    }
    out.sort_by(|a, b| a.mount_path.cmp(&b.mount_path));
    Ok(Json(out))
}

#[derive(Serialize)]
struct MemberDto {
    id: i64,
    location: String,
    filename: String,
    volume_id: String,
    volume_label: String,
    size_bytes: i64,
    category: String,
    created_time: Option<i64>,
    modified_time: Option<i64>,
    status: String,
    is_loose: bool,
    mounted: bool,
}

#[derive(Deserialize)]
struct CopiesParams {
    hash: String,
}

#[derive(Deserialize)]
struct VolumeErrorParams {
    bucket: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Serialize)]
struct VolumeErrorsDto {
    totals: crate::catalog::scan_errors::Completeness,
    rows: Vec<crate::catalog::scan_errors::ScanErrorRow>,
}

// NOTE: `axum::extract::Path` is imported in this file as `AxPath` (web.rs:6), because
// `std::path::PathBuf` is also in scope. Use `AxPath` -- writing `Path` here will not compile.
async fn api_volume_errors(
    State(state): State<AppState>,
    AxPath(volume_id): AxPath<String>,
    Query(p): Query<VolumeErrorParams>,
) -> Result<Json<VolumeErrorsDto>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    // Bounded by default: a badly failing drive can hold a lot of rows, and the totals are the
    // headline anyway.
    let limit = p.limit.unwrap_or(200).min(1000);
    Ok(Json(VolumeErrorsDto {
        totals: cat.volume_completeness(&volume_id).map_err(err500)?,
        rows: cat
            .volume_scan_errors(
                &volume_id,
                p.bucket.as_deref(),
                p.kind.as_deref(),
                limit,
                p.offset.unwrap_or(0),
            )
            .map_err(err500)?,
    }))
}

/// Every active copy of one content hash, wherever it lives — including on drives that are not
/// currently connected.
///
/// Browse can only highlight rows it has already loaded, so on a truncated result set clicking a
/// duplicate looked like "2 copies" when there might be twenty (#30). Under-reporting copies is the
/// one thing a duplicate finder must never do, so the count comes from the catalogue, not the page.
async fn api_copies(
    State(state): State<AppState>,
    Query(p): Query<CopiesParams>,
) -> Result<Json<Vec<MemberDto>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let labels: std::collections::HashMap<String, String> = cat
        .volume_stats()
        .map_err(err500)?
        .into_iter()
        .map(|(id, label, _, _)| (id, label))
        .collect();
    let mounts = state.mounts.snapshot();
    let out = cat
        .active_copies(&p.hash)
        .map_err(err500)?
        .into_iter()
        .map(|f| {
            let mounted = mounts.contains_key(&f.volume_id);
            MemberDto {
                id: f.id,
                location: f.display_location(),
                filename: f.filename.clone(),
                volume_label: labels.get(&f.volume_id).cloned().unwrap_or_default(),
                volume_id: f.volume_id,
                size_bytes: f.size_bytes,
                category: f.category.as_str().to_string(),
                created_time: f.created_time,
                modified_time: f.modified_time,
                status: f.status.as_str().to_string(),
                is_loose: f.container_chain.is_none(),
                mounted,
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Serialize)]
struct GroupDto {
    content_hash: String,
    copies: i64,
    size_bytes: i64,
    reclaimable_bytes: i64,
    suggested_keep_id: i64,
    members: Vec<MemberDto>,
}

#[derive(Serialize)]
struct TotalsDto {
    groups: i64,
    reclaimable_bytes: i64,
    groups_all: i64,
    reclaimable_all_bytes: i64,
    archive_locked_bytes: i64,
}

#[derive(Serialize)]
struct CursorDto {
    reclaimable_bytes: i64,
    content_hash: String,
}

#[derive(Serialize)]
struct DuplicatesDto {
    /// Only on the first page of a filter. The totals are three full aggregate passes (~4 s on a
    /// 1.8M-row catalogue) and do not change while paging, so continuation pages omit them and the
    /// client keeps the ones it already has.
    totals: Option<TotalsDto>,
    groups: Vec<GroupDto>,
    next: Option<CursorDto>,
}

#[derive(Deserialize, Default)]
struct DuplicatesParams {
    min_size: Option<i64>,
    limit: Option<usize>,
    after_reclaimable: Option<i64>,
    after_hash: Option<String>,
}

/// One ranked page of duplicate groups plus the honest totals. Bounded by design: a page costs two
/// queries regardless of catalogue size.
async fn api_duplicates(
    State(state): State<AppState>,
    Query(p): Query<DuplicatesParams>,
) -> Result<Json<DuplicatesDto>, (axum::http::StatusCode, String)> {
    use crate::catalog::dedup::{DuplicateCursor, DEFAULT_MIN_SIZE};
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let min_size = p.min_size.unwrap_or(DEFAULT_MIN_SIZE);
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let after = match (p.after_reclaimable, p.after_hash) {
        (Some(reclaimable_bytes), Some(content_hash)) => Some(DuplicateCursor {
            reclaimable_bytes,
            content_hash,
        }),
        _ => None,
    };

    let totals = match after {
        Some(_) => None,
        None => Some(cat.duplicate_totals(min_size).map_err(err500)?),
    };
    let groups = cat
        .duplicate_groups_ranked(min_size, limit, after.as_ref())
        .map_err(err500)?;
    let hashes: Vec<String> = groups.iter().map(|g| g.content_hash.clone()).collect();
    let mut members = cat.duplicate_members_for(&hashes).map_err(err500)?;

    let labels: std::collections::HashMap<String, String> = cat
        .volume_stats()
        .map_err(err500)?
        .into_iter()
        .map(|(id, label, _, _)| (id, label))
        .collect();
    let mounts = state.mounts.snapshot();

    let next = groups.last().map(|g| CursorDto {
        reclaimable_bytes: g.reclaimable_bytes,
        content_hash: g.content_hash.clone(),
    });

    let out_groups = groups
        .into_iter()
        .map(|g| {
            let ms = members.remove(&g.content_hash).unwrap_or_default();
            let suggested_keep_id = ms
                .iter()
                .find(|m| m.is_suggested_keep)
                .map(|m| m.record.id)
                .unwrap_or(0);
            let members = ms
                .into_iter()
                .map(|m| {
                    let f = m.record;
                    let mounted = mounts.contains_key(&f.volume_id);
                    MemberDto {
                        id: f.id,
                        location: f.display_location(),
                        filename: f.filename.clone(),
                        volume_label: labels.get(&f.volume_id).cloned().unwrap_or_default(),
                        volume_id: f.volume_id,
                        size_bytes: f.size_bytes,
                        category: f.category.as_str().to_string(),
                        created_time: f.created_time,
                        modified_time: f.modified_time,
                        status: f.status.as_str().to_string(),
                        is_loose: f.container_chain.is_none(),
                        mounted,
                    }
                })
                .collect();
            GroupDto {
                content_hash: g.content_hash,
                copies: g.copies,
                size_bytes: g.size_bytes,
                reclaimable_bytes: g.reclaimable_bytes,
                suggested_keep_id,
                members,
            }
        })
        .collect();

    Ok(Json(DuplicatesDto {
        totals: totals.map(|t| TotalsDto {
            groups: t.groups,
            reclaimable_bytes: t.reclaimable_bytes,
            groups_all: t.groups_all,
            reclaimable_all_bytes: t.reclaimable_all_bytes,
            archive_locked_bytes: t.archive_locked_bytes,
        }),
        groups: out_groups,
        next,
    }))
}

const PREVIEW_MAX_DIM: u32 = 320;

/// Photo thumbnail for a file that is: a photo, mounted, and either loose or a top-level
/// archive entry (no nested-archive chain). Anything else — or a decode failure — is a 404,
/// never a panic.
async fn api_preview(State(state): State<AppState>, AxPath(id): AxPath<i64>) -> Response {
    let not_found = |msg: &str| (StatusCode::NOT_FOUND, msg.to_string()).into_response();

    let cat = match Catalog::open_readonly(&state.catalog_path) {
        Ok(c) => c,
        Err(e) => return err500(e).into_response(),
    };
    let rec = match cat.get_file(id) {
        Ok(Some(r)) => r,
        Ok(None) => return not_found("no such file"),
        Err(e) => return err500(e).into_response(),
    };
    if rec.category != crate::category::Category::Photo {
        return not_found("preview only for photos");
    }
    let Some(mount) = state.mounts.resolve(&rec.volume_id) else {
        return not_found("drive not connected");
    };

    let bytes = match &rec.container_chain {
        None => std::fs::read(mount.join(&rec.relative_path)).ok(),
        Some(chain) if !chain.contains(" › ") => {
            crate::image_preview::read_zip_entry(&mount.join(&rec.relative_path), chain).ok()
        }
        Some(_) => return not_found("nested-archive preview not supported"),
    };
    let Some(bytes) = bytes else {
        return not_found("file unavailable");
    };

    match crate::image_preview::thumbnail_jpeg(&bytes, PREVIEW_MAX_DIM) {
        Ok(jpeg) => ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response(),
        Err(_) => not_found("not a decodable image"),
    }
}

#[derive(Deserialize)]
struct QuarantineReq {
    quarantine_ids: Vec<i64>,
}

#[derive(Serialize, Default)]
struct QuarantineQueuedFilesDto {
    /// How many of the requested ids are now queued for the worker (including any that a
    /// previous click had already queued).
    queued: usize,
    /// Position of the last job enqueued; 0 means it starts next.
    position: usize,
    /// Ids that resolve to nothing, or that were already queued. Not an error.
    skipped: usize,
    /// Volumes that are not currently mounted. Knowable now, so it is answered now rather than
    /// leaving the reviewer to discover it from a status poll.
    unmounted_volumes: Vec<String>,
}

/// Enqueue single-file quarantines and return immediately.
///
/// Returns an acknowledgement rather than an outcome. The work has not happened when this response
/// is written, so reporting `quarantined`/`skipped` counts here would be a lie — those numbers
/// arrive through `/api/quarantine/status` once the worker has run.
///
/// All destructive safety (marker check, disk-aware last-copy guard, rename-only) lives in
/// `quarantine::quarantine_files` and runs inside the worker, immediately before it acts — which is
/// the only point at which those checks are not already stale.
async fn api_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<QuarantineReq>,
) -> Result<Json<QuarantineQueuedFilesDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;

    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;

    // Group requested ids by their volume; ids that don't resolve to a file are counted skipped.
    let mut by_volume: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    let mut out = QuarantineQueuedFilesDto::default();
    for id in &body.quarantine_ids {
        match cat.get_file(*id).map_err(err500)? {
            Some(rec) => by_volume.entry(rec.volume_id).or_default().push(*id),
            None => out.skipped += 1,
        }
    }

    let mounts = state.mounts.snapshot();
    for (volume_id, ids) in by_volume {
        if !mounts.contains_key(&volume_id) {
            out.skipped += ids.len();
            out.unmounted_volumes.push(volume_id);
            continue;
        }
        let n = ids.len();
        match state.quarantine_queue.enqueue_files(volume_id, ids) {
            Some(position) => {
                out.queued += n;
                out.position = position;
            }
            // Every id was already queued. Not a failure, and not something to report as an error:
            // the reviewer double-clicked, and the decision is already on its way.
            None => out.skipped += n,
        }
    }

    Ok(Json(out))
}

#[derive(Deserialize)]
struct RepackReq {
    entry_id: i64,
}

#[derive(Serialize)]
struct RepackResultDto {
    removed_entry: String,
    retained_entries: usize,
}

/// Remove one entry from its containing archive (Case 4). All destructive safety (marker gate,
/// disk-aware survivor guard, verify-before-swap, two recovery nets) lives in `repack::repack_entry`;
/// this handler is just the CSRF gate plus resolving the entry's volume to a mounted drive.
async fn api_repack(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<RepackReq>,
) -> Result<Json<RepackResultDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;

    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;
    let rec = cat
        .get_file(body.entry_id)
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "no such entry".to_string()))?;
    let mount = state
        .mounts
        .resolve(&rec.volume_id)
        .ok_or((StatusCode::CONFLICT, "drive not connected".to_string()))?;
    let now = now_secs()?;
    let out = crate::repack::repack_entry(&cat, &mount, &rec.volume_id, body.entry_id, now)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Snapshot the catalog this request actually mutated (best-effort; a snapshot failure
    // shouldn't fail the request).
    snapshot_best_effort(&state, now);
    Ok(Json(RepackResultDto {
        removed_entry: out.removed_entry,
        retained_entries: out.retained_entries,
    }))
}

#[derive(Serialize)]
struct TreeMemberDto {
    volume_id: String,
    volume_label: String,
    path: String,
    total_bytes: i64,
    /// True when this folder lives inside an archive. Such a folder CANNOT be quarantined -- a file
    /// inside a zip cannot be renamed out of it -- so the UI must offer no delete for it.
    needs_repack: bool,
    archive: Option<String>,
    /// Whether the drive is currently connected. A group is still worth showing for an unplugged
    /// drive (the catalogue knows it), but nothing can be moved until it is attached.
    mounted: bool,
}

#[derive(Serialize)]
struct TreeGroupDto {
    dir_hash: String,
    reclaimable_bytes: i64,
    file_count: i64,
    /// True when at least one copy is a loose folder, i.e. something can actually be quarantined
    /// while another copy survives.
    ///
    /// On the first real 4 TB scan, 73% of rendered folder entries were inside archives and could
    /// not be acted on at all (#59). A correct list that is mostly unactionable is not a worklist,
    /// so the client ranks and groups on this rather than showing everything at one level.
    actionable: bool,
    members: Vec<TreeMemberDto>,
}

#[derive(Serialize)]
struct TreeDuplicatesDto {
    groups: Vec<TreeGroupDto>,
    /// Total groups available, so the client knows whether asking for more is worthwhile.
    total: usize,
    /// Groups before this page, echoed back so a client cannot lose its place.
    offset: usize,
}

#[derive(Deserialize)]
struct PageParams {
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Maximal identical-folder groups, ranked by reclaimable bytes.
///
/// This is the primary duplicate view: on the live catalogue it turns 125,977 individual decisions
/// into about 1,458. The per-file list remains for what does not collapse.
async fn api_tree_duplicates(
    State(state): State<AppState>,
    Query(p): Query<PageParams>,
) -> Result<Json<TreeDuplicatesDto>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    // `effective_labels`, NOT `volume_stats`: the latter returns the DETECTED label, so two
    // drives both first seen as `D:\` render as identical rows with identical buttons and the
    // user cannot tell which physical drive a Quarantine acts on (#62). The name set on the
    // Drives page exists precisely to disambiguate this, and this view was ignoring it.
    let labels = cat.effective_labels().map_err(err500)?;
    let mounts = state.mounts.snapshot();
    let mut groups: Vec<TreeGroupDto> = cat
        .tree_duplicate_groups()
        .map_err(err500)?
        .into_iter()
        .map(|g| {
            let members: Vec<TreeMemberDto> = g
                .members
                .iter()
                .map(|m| TreeMemberDto {
                    volume_label: labels
                        .get(&m.volume_id)
                        .cloned()
                        .unwrap_or_else(|| m.volume_id.clone()),
                    mounted: mounts.contains_key(&m.volume_id),
                    needs_repack: m.archive_container().is_some(),
                    archive: m.archive_container().map(|s| s.to_string()),
                    volume_id: m.volume_id.clone(),
                    path: m.path.clone(),
                    total_bytes: m.total_bytes,
                })
                .collect();
            TreeGroupDto {
                dir_hash: g.dir_hash,
                reclaimable_bytes: g.reclaimable_bytes,
                // Every member of a group holds the same tree, so any member's count describes all.
                file_count: g.members.first().map(|m| m.file_count).unwrap_or(0),
                actionable: members.iter().any(|m| !m.needs_repack),
                members,
            }
        })
        .collect();

    // Actionable first, then by size within each half. Sorting here rather than in the client keeps
    // the two consumers -- page and any future CLI -- from disagreeing about what "first" means.
    groups.sort_by(|a, b| {
        b.actionable
            .cmp(&a.actionable)
            .then(b.reclaimable_bytes.cmp(&a.reclaimable_bytes))
    });

    // Page AFTER sorting, so the first page is the most valuable groups rather than an arbitrary
    // slice. On the real catalogue the whole set is 2.6 MB and the top 20 groups carry 63% of all
    // reclaimable space, so the default page is where nearly all the value is.
    let total = groups.len();
    let offset = p.offset.unwrap_or(0).min(total);
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let groups = groups.into_iter().skip(offset).take(limit).collect();
    Ok(Json(TreeDuplicatesDto {
        groups,
        total,
        offset,
    }))
}

#[derive(Deserialize)]
struct QuarantineTreeReq {
    volume_id: String,
    path: String,
}

#[derive(Serialize)]
struct QuarantineQueuedDto {
    queued: bool,
    /// How many items are ahead of this one; 0 means it starts next.
    position: usize,
}

/// Move one redundant folder into `_ToDelete` with a single rename.
///
/// The mount is resolved from the volume id server-side rather than accepted from the client: a
/// caller-supplied path would let a request rename a directory on any drive it could name.
/// Enqueue a folder quarantine and return immediately.
///
/// Returns the queue position rather than the outcome. Reviewing 1,201 folders one blocking request
/// at a time was the complaint this answers (#66): the reviewer confirms an item and moves on, and
/// the worker drains the queue in order.
///
/// Nothing is validated here beyond the CSRF gate. Every safety check -- the drive marker, the tree
/// still being wholly `active`, the destination not already claimed -- happens in
/// `tree_quarantine::quarantine_tree` immediately before it acts, which is the only place they are
/// not already stale.
async fn api_quarantine_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<QuarantineTreeReq>,
) -> Result<Json<QuarantineQueuedDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let position = state
        .quarantine_queue
        .enqueue_tree(body.volume_id.clone(), body.path.clone());
    Ok(Json(QuarantineQueuedDto {
        queued: true,
        position,
    }))
}

async fn api_quarantine_status(
    State(state): State<AppState>,
) -> Json<crate::quarantine_queue::QuarantineStatus> {
    Json(state.quarantine_queue.status())
}

#[derive(serde::Serialize)]
struct SettingsDto {
    max_archive_depth: usize,
    archive_buffer_max_bytes: u64,
    archive_total_buffer_bytes: u64,
    archive_entry_max_bytes: Option<u64>,
    archive_ratio_cap: u64,
    archive_deny_extensions: Vec<String>,
    archive_allow_extensions: Vec<String>,
}

/// The effective configuration, re-read from disk. Shared by both handlers so a write responds
/// with what is actually in force rather than echoing the request back.
fn effective_settings() -> Result<SettingsDto, (StatusCode, String)> {
    let cfg = crate::config::Config::default_paths().map_err(err500)?;
    Ok(SettingsDto {
        max_archive_depth: cfg.max_archive_depth,
        archive_buffer_max_bytes: cfg.archive_buffer_max_bytes,
        archive_total_buffer_bytes: cfg.archive_total_buffer_bytes,
        archive_entry_max_bytes: cfg.archive_entry_max_bytes,
        archive_ratio_cap: cfg.archive_ratio_cap,
        archive_deny_extensions: cfg.archive_deny_extensions,
        archive_allow_extensions: cfg.archive_allow_extensions,
    })
}

async fn api_settings_get() -> Result<Json<SettingsDto>, (StatusCode, String)> {
    Ok(Json(effective_settings()?))
}

/// Unfamiliar zip-format extensions found during scans, aggregated for the user to approve
/// ("descend" -> treat as an archive) or dismiss ("document" -> treat as a document/package).
///
/// Two -- and only two -- cases degrade to an empty list instead of a 500, both meaning "there is
/// truly nothing to report yet":
///   1. No catalog file exists at all (nothing has been scanned yet).
///   2. The catalog exists but predates this branch, so `pending_archive_formats` hasn't been
///      created -- `Catalog::open_readonly` runs no schema DDL, and the table appears the next
///      time anything write-opens the catalog (the scanner, or `api_resolve_format`).
///
/// Any other failure to open or query the catalog -- a locked file, a permissions error, a path
/// that is not a database at all -- surfaces as a genuine 500. This endpoint's whole job is to
/// warn the user about files the scanner could not classify, so swallowing a real error here
/// would silently read as "nothing pending", indistinguishable from a clean drive.
async fn api_pending_formats(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::catalog::pending_formats::PendingFormat>>, (StatusCode, String)> {
    if !state.catalog_path.exists() {
        return Ok(Json(Vec::new()));
    }
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    match cat.pending_formats() {
        Ok(rows) => Ok(Json(rows)),
        Err(e) if e.to_string().contains("no such table") => Ok(Json(Vec::new())),
        Err(e) => Err(err500(e)),
    }
}

#[derive(serde::Deserialize)]
struct ResolveFormat {
    extension: String,
    /// "descend" -> allow-list; "document" -> deny-list.
    action: String,
}

/// Merges the resolved extension into the stored settings (never overwrites the other fields),
/// then clears the pending rows for it. Idempotent: resolving the same extension twice adds it
/// only once, and seeds from the EFFECTIVE list (compiled-in default when nothing is stored) so
/// one click cannot silently discard the rest of the deny-list.
async fn api_resolve_format(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResolveFormat>,
) -> Result<Json<Vec<crate::catalog::pending_formats::PendingFormat>>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    // "" is a legitimate value here, not a malformed request: an extensionless zip-format file
    // records a pending row with extension == "" (src/scanner.rs, `descent_ext`), and resolving
    // that row must round-trip the same as any other extension.
    let ext = body.extension.to_ascii_lowercase();
    let cfg = crate::config::Config::default_paths().map_err(err500)?;
    let path = cfg.settings_path();
    // Merge, never overwrite: the other settings must survive.
    let mut s = crate::config::load_settings(&path);
    let list = match body.action.as_str() {
        "descend" => &mut s.archive_allow_extensions,
        "document" => &mut s.archive_deny_extensions,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown action {other:?}; expected \"descend\" or \"document\""),
            ))
        }
    };
    // The trap: when the stored settings have no explicit list, the EFFECTIVE list is the
    // compiled-in default (`cfg`, already resolved). Seed from that -- not an empty vec -- before
    // appending, or resolving one format silently discards the whole default deny-list.
    let mut v = list.take().unwrap_or_else(|| match body.action.as_str() {
        "document" => cfg.archive_deny_extensions.clone(),
        _ => cfg.archive_allow_extensions.clone(),
    });
    if !v.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
        v.push(ext.clone());
    }
    *list = Some(v);
    crate::config::save_settings(&path, &s).map_err(err500)?;

    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;
    // "descend" needs the next ordinary scan to actually re-open these files -- SC3. The
    // incremental skip path never opens a file whose cached (size, mtime) still matches disk, so
    // without this a plain `force=false` rescan would never reach `descend_archive` and approving
    // an extension would be a silent no-op (see F-1, archive-descent-policy review). "document"
    // needs none of this: the file is already catalogued whole, which is what the deny-list means.
    if body.action == "descend" {
        for (volume_id, relative_path) in cat.pending_format_paths(&ext).map_err(err500)? {
            cat.invalidate_scan_fingerprint(&volume_id, &relative_path)
                .map_err(err500)?;
        }
    }
    cat.clear_pending_format(&ext).map_err(err500)?;
    Ok(Json(cat.pending_formats().map_err(err500)?))
}

/// A quarter of RAM: the scan leans on the OS file cache, and `browse` runs the web server in this
/// same process. Buffering more than this trades a working scan for a bigger buffer.
fn memory_ceiling() -> Option<u64> {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    match sys.total_memory() {
        0 => None,
        total => Some(total / 4),
    }
}

/// `before` is the currently-EFFECTIVE config (defaults already resolved) that `s` would replace.
/// The memory-ceiling check only fires for a field that is actually being RAISED above that
/// baseline: F7 -- the compiled-in default `archive_buffer_max_bytes` (2 GiB) already exceeds
/// RAM/4 on a machine with under 8 GiB, so checking every save unconditionally would refuse EVERY
/// save on such a machine, including ones that only touch the ratio cap and leave the buffer
/// fields untouched. A value that is unchanged, or lowered, cannot make memory pressure worse than
/// it already was, so it must always be editable.
fn validate(s: &crate::config::Settings, before: &crate::config::Config) -> Result<(), String> {
    // Shared with load-time validation in config.rs: a hand-edited settings.json must be rejected
    // (per field) by the exact same rules the HTTP boundary enforces.
    if let Some((field, reason)) = crate::config::check_ranges(s).into_iter().next() {
        return Err(format!("{field} {reason}"));
    }
    if let Some(ceiling) = memory_ceiling() {
        for (name, v, prev) in [
            (
                "archive_buffer_max_bytes",
                s.archive_buffer_max_bytes,
                before.archive_buffer_max_bytes,
            ),
            (
                "archive_total_buffer_bytes",
                s.archive_total_buffer_bytes,
                before.archive_total_buffer_bytes,
            ),
        ] {
            if let Some(v) = v {
                if v > ceiling && v > prev {
                    return Err(format!(
                        "{name} of {v} bytes exceeds a quarter of system memory ({ceiling} bytes); \
                         buffering that much would starve the file cache the scan depends on"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Merge a POST body onto the currently-stored settings: a field the request omits (`None`) keeps
/// whatever is stored, a field it sets (`Some(..)`) overwrites -- including `Some(None)` on
/// `archive_entry_max_bytes`, which means "explicitly unlimited" and must overwrite too, not be
/// treated as absence.
///
/// Two different meanings of "absent" meet here: in the STORED file, `None` means "unset, use the
/// compiled-in default"; in the REQUEST, `None` means "omitted, keep whatever is stored". Without
/// this merge, a partial POST (e.g. one field from a `curl` call) would silently reset every other
/// field to its hardcoded default -- exactly what Task 4's "every field is optional" contract was
/// meant to prevent.
fn merge_settings(
    stored: crate::config::Settings,
    req: crate::config::Settings,
) -> crate::config::Settings {
    crate::config::Settings {
        max_archive_depth: req.max_archive_depth.or(stored.max_archive_depth),
        archive_buffer_max_bytes: req
            .archive_buffer_max_bytes
            .or(stored.archive_buffer_max_bytes),
        archive_total_buffer_bytes: req
            .archive_total_buffer_bytes
            .or(stored.archive_total_buffer_bytes),
        archive_entry_max_bytes: req
            .archive_entry_max_bytes
            .or(stored.archive_entry_max_bytes),
        archive_ratio_cap: req.archive_ratio_cap.or(stored.archive_ratio_cap),
        archive_deny_extensions: req
            .archive_deny_extensions
            .or(stored.archive_deny_extensions),
        archive_allow_extensions: req
            .archive_allow_extensions
            .or(stored.archive_allow_extensions),
    }
}

async fn api_settings_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::config::Settings>,
) -> Result<Json<SettingsDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let cfg = crate::config::Config::default_paths().map_err(err500)?;
    let path = cfg.settings_path();
    let stored = crate::config::load_settings(&path);
    let merged = merge_settings(stored, body);
    // Validate the MERGED result, not the request alone: a request that omits
    // archive_total_buffer_bytes could otherwise slip an archive_buffer_max_bytes past the
    // per-archive-vs-total rule by comparing it against nothing. `cfg` is the config in effect
    // BEFORE this write, so the memory-ceiling check in `validate` can tell a raise from a no-op.
    validate(&merged, &cfg).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    crate::config::save_settings(&path, &merged).map_err(err500)?;
    Ok(Json(effective_settings()?))
}

#[derive(Deserialize)]
struct ForgetReq {
    volume_id: String,
}

#[derive(Serialize)]
struct ForgetResultDto {
    removed_files: usize,
}

/// Remove a volume's catalog rows entirely (files on disk untouched; a rescan re-adds them).
/// All destructive safety lives in `Catalog::forget_volume` (a same-transaction delete); this
/// handler is just the CSRF gate plus a best-effort pre-mutation snapshot.
async fn api_forget_drive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<ForgetReq>,
) -> Result<Json<ForgetResultDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;
    let now = now_secs()?;
    snapshot_best_effort(&state, now);
    let removed = cat.forget_volume(&body.volume_id, now).map_err(err500)?;
    Ok(Json(ForgetResultDto {
        removed_files: removed,
    }))
}

#[derive(Deserialize)]
struct RenameReq {
    volume_id: String,
    name: Option<String>,
    description: Option<String>,
}

#[derive(Serialize)]
struct RenameResultDto {
    name: String,
}

/// Set a volume's custom display name and/or description. All persistence lives in
/// `Catalog::set_volume_meta`; this handler is just the CSRF gate plus resolving the effective
/// name to return.
async fn api_rename_drive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<RenameReq>,
) -> Result<Json<RenameResultDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;
    let now = now_secs()?;
    cat.set_volume_meta(
        &body.volume_id,
        body.name.as_deref(),
        body.description.as_deref(),
        now,
    )
    .map_err(err500)?;
    let name = cat
        .effective_labels()
        .map_err(err500)?
        .get(&body.volume_id)
        .cloned()
        .unwrap_or_else(|| body.volume_id.clone());
    Ok(Json(RenameResultDto { name }))
}

#[derive(Serialize)]
struct PurgeAllResultDto {
    purged_volumes: usize,
    files_purged: usize,
    bytes_reclaimed: i64,
    skipped_unmounted: Vec<String>,
    errors: Vec<String>,
}

/// Purge every mounted volume that has reclaimable quarantine (Task 6's `purge_all`). All
/// destructive safety lives in `purge::purge_volume` (called per-volume); this handler is just
/// the CSRF gate plus a best-effort pre-mutation snapshot.
async fn api_purge_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PurgeAllResultDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;
    let now = now_secs()?;
    snapshot_best_effort(&state, now);
    let out = crate::purge::purge_all(&cat, &state.mounts.snapshot(), now).map_err(err500)?;
    Ok(Json(PurgeAllResultDto {
        purged_volumes: out.purged.len(),
        files_purged: out.purged.iter().map(|(_, f, _)| f).sum(),
        bytes_reclaimed: out.purged.iter().map(|(_, _, b)| b).sum(),
        skipped_unmounted: out.skipped_unmounted,
        errors: out.errors,
    }))
}

#[derive(Deserialize)]
struct ScanReq {
    path: String,
    force: bool,
}

#[derive(Serialize)]
struct ScanEnqueuedDto {
    queued_position: usize,
}

/// Enqueue a background scan of `path`. This handler is just the CSRF gate plus input
/// validation; the actual scan runs one-at-a-time in `ScanQueue`'s worker task so the request
/// returns immediately instead of blocking on a potentially slow drive walk.
async fn api_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<ScanReq>,
) -> Result<Json<ScanEnqueuedDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;

    if body.path.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path is required".into()));
    }
    let path = std::path::PathBuf::from(body.path.trim());
    let pos = state.scan_queue.enqueue(path, body.force);
    Ok(Json(ScanEnqueuedDto {
        queued_position: pos,
    }))
}

async fn api_scan_status(State(state): State<AppState>) -> Json<crate::scan_queue::StatusSnapshot> {
    Json(state.scan_queue.status())
}

/// Ask the running scan to stop. Idempotent, and harmless when nothing is running.
///
/// `stopping: false` means there was nothing to stop, not that the request failed -- the CLI
/// stop button treats that as a normal, non-error outcome.
async fn api_scan_stop(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let stopping = state.scan_queue.request_stop();
    Ok(Json(serde_json::json!({ "stopping": stopping })))
}

#[derive(Deserialize, Default)]
struct ScanRunsParams {
    limit: Option<usize>,
}

/// Recent scan runs with their phase breakdown. Read-only — no CSRF surface.
async fn api_scan_runs(
    State(state): State<AppState>,
    Query(p): Query<ScanRunsParams>,
) -> Result<Json<Vec<crate::catalog::scan_runs::ScanRun>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200);
    Ok(Json(cat.recent_scan_runs(limit).map_err(err500)?))
}

#[derive(Serialize)]
struct PickFolderDto {
    path: Option<String>,
}

/// A borrowed Win32 `HWND` we can hand to `rfd::FileDialog::set_parent`, so the folder dialog opens
/// owned by (and on top of) the browser window instead of behind it. The raw handle value is just an
/// `isize`, which is `Send`; we rebuild the window handle inside the blocking closure.
#[cfg(windows)]
struct HwndOwner(isize);
#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for HwndOwner {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let hwnd = std::num::NonZeroIsize::new(self.0)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window_handle::Win32WindowHandle::new(hwnd);
        // SAFETY: the HWND is valid for the lifetime of the (blocking, synchronous) dialog call.
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(
                handle,
            ))
        })
    }
}
#[cfg(windows)]
impl raw_window_handle::HasDisplayHandle for HwndOwner {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let raw = raw_window_handle::RawDisplayHandle::Windows(
            raw_window_handle::WindowsDisplayHandle::new(),
        );
        // SAFETY: the Windows display handle carries no pointer; it is valid for any lifetime.
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(raw) })
    }
}

/// Once the native folder dialog appears, move it to the centre of its monitor's work area.
/// The dialog is parented to the browser (so it comes to the front), but that also centres it over
/// the browser rather than the screen; this short-lived watcher repositions it to the monitor centre.
/// It finds the one visible top-level window owned by our own process (the dialog) and moves it.
#[cfg(windows)]
fn center_dialog_when_shown() {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, SetWindowPos,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    unsafe extern "system" fn find_ours(hwnd: HWND, lparam: LPARAM) -> i32 {
        let out = &mut *(lparam as *mut HWND);
        if IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == GetCurrentProcessId() {
                *out = hwnd;
                return 0; // stop enumerating
            }
        }
        1 // keep going
    }

    std::thread::spawn(|| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2500);
        loop {
            let mut found: HWND = std::ptr::null_mut();
            unsafe { EnumWindows(Some(find_ours), &mut found as *mut HWND as LPARAM) };
            if !found.is_null() {
                unsafe {
                    let mut wr: RECT = std::mem::zeroed();
                    let mut mi: MONITORINFO = std::mem::zeroed();
                    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                    let mon = MonitorFromWindow(found, MONITOR_DEFAULTTONEAREST);
                    if GetWindowRect(found, &mut wr) != 0 && GetMonitorInfoW(mon, &mut mi) != 0 {
                        let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
                        let work = mi.rcWork;
                        let x = work.left + ((work.right - work.left) - w) / 2;
                        let y = work.top + ((work.bottom - work.top) - h) / 2;
                        SetWindowPos(
                            found,
                            std::ptr::null_mut(),
                            x,
                            y,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    });
}

/// Open the native OS folder-picker dialog and return the chosen path (or `null` on cancel).
/// The dialog call is blocking, so it runs on a `spawn_blocking` thread rather than the async
/// runtime. This handler is just the CSRF gate plus that thread hop. On Windows the dialog is
/// parented to the current foreground window (the browser) so it opens on top, and a watcher
/// re-centres it on the monitor once it appears.
async fn api_pick_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PickFolderDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;

    #[cfg(windows)]
    let owner: isize =
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as isize };

    let picked = tokio::task::spawn_blocking(move || {
        let dialog = rfd::FileDialog::new().set_title("Choose a drive or folder to scan");
        #[cfg(windows)]
        let dialog = if owner != 0 {
            dialog.set_parent(&HwndOwner(owner))
        } else {
            dialog
        };
        #[cfg(windows)]
        center_dialog_when_shown();
        dialog.pick_folder()
    })
    .await
    .map_err(err500)?;
    Ok(Json(PickFolderDto {
        path: picked.map(|p| p.display().to_string()),
    }))
}

/// Serve the browse UI on 127.0.0.1 with an OS-assigned free port until the process is stopped.
pub async fn serve(catalog_path: PathBuf, open: bool) -> anyhow::Result<()> {
    let state = AppState::new_live(catalog_path);
    tokio::spawn(state.scan_queue.clone().run_worker());
    tokio::spawn(state.quarantine_queue.clone().run_worker());
    let app = build_router_with(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let url = format!("http://{}", listener.local_addr()?);
    println!("CleanUpStorages browse UI at {url}");
    println!("(read-only; press Ctrl+C to stop)");
    if open {
        if let Err(e) = open_browser(&url) {
            eprintln!("could not open a browser automatically ({e}); open {url} yourself");
        }
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// Best-effort open of `url` in the user's default browser (never fatal).
fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// Limits for tests: the compiled-in defaults, with NO ambient environment read.
    fn test_limits() -> crate::archive::ArchiveLimits {
        crate::archive::ArchiveLimits {
            max_depth: 8,
            buffer_max_bytes: 2 * 1024 * 1024 * 1024,
            total_buffer_bytes: 2 * 1024 * 1024 * 1024,
            entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
            ratio_cap: 10_000,
            deny_extensions: crate::config::DEFAULT_DENY
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allow_extensions: Vec::new(),
        }
    }

    #[test]
    fn app_state_new_live_has_token_and_live_mounts() {
        let s = AppState::new_live(PathBuf::from("x.db"));
        assert!(!s.csrf_token.is_empty());
        assert!(matches!(
            s.mounts,
            crate::mounts::MountResolver::Live { .. }
        ));
    }

    #[tokio::test]
    async fn index_returns_200_html() {
        let app = build_router(PathBuf::from("unused.db"));
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("CleanUpStorages"));
    }

    fn seed_catalog() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "Test HDD".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let mut f = crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: "docs/thesis.pdf".into(),
                filename: "thesis.pdf".into(),
                extension: "pdf".into(),
                size_bytes: 123,
                content_hash: "h1".into(),
                created_time: None,
                modified_time: Some(50),
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&f, 100).unwrap();
            f.relative_path = "old.zip".into();
            f.filename = "inner.jpg".into();
            f.extension = "jpg".into();
            f.container_chain = Some("inner.jpg".into());
            f.category = crate::category::Category::Photo;
            f.content_hash = "h2".into();
            cat.upsert_archive_entry(
                "vol-1",
                "old.zip",
                &crate::archive::ArchiveEntry {
                    container_chain: "inner.jpg".into(),
                    filename: "inner.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 9,
                    content_hash: "h2".into(),
                },
                None,
                100,
            )
            .unwrap();
        }
        (tmp, db)
    }

    async fn get_json(db: &std::path::Path, uri: &str) -> serde_json::Value {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(db.to_path_buf());
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK, "uri {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), 5_000_000)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn api_search_returns_hits_with_location() {
        let (_t, db) = seed_catalog();
        let v = get_json(&db, "/api/search?q=thesis").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["location"], "docs/thesis.pdf");
        assert_eq!(arr[0]["volume_id"], "vol-1");
    }

    #[tokio::test]
    async fn api_search_shows_archive_chain_in_location() {
        let (_t, db) = seed_catalog();
        let v = get_json(&db, "/api/search?q=inner").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["location"], "old.zip › inner.jpg");
        assert_eq!(arr[0]["category"], "photo");
    }

    #[tokio::test]
    async fn api_search_enriches_label_hash_and_copies() {
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/search").await; // empty query -> all files
        let arr = v.as_array().unwrap();
        assert!(arr.len() >= 2);
        for h in arr {
            assert_eq!(h["volume_label"], "Photos HDD"); // friendly name, not the id
            assert_eq!(h["content_hash"], "DUP");
            assert_eq!(h["copies"], 2); // both are duplicated (2 active copies)
        }
    }

    #[tokio::test]
    async fn api_copies_reports_every_copy_including_disconnected_drives() {
        // The point of #30: the answer must come from the catalogue, not from whatever rows a
        // truncated page happened to load, and it must include drives that are not plugged in.
        let (_t, db, state) = seed_dupes(); // two active copies of hash DUP on vol-1 (mounted)
        {
            let cat = Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-elsewhere".into(),
                label: "Offline HDD".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            cat.upsert_file(
                &crate::catalog::models::NewFile {
                    volume_id: "vol-elsewhere".into(),
                    relative_path: "far/away.jpg".into(),
                    filename: "away.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 10,
                    content_hash: "DUP".into(),
                    created_time: Some(1),
                    modified_time: Some(1),
                    accessed_time: None,
                    category: crate::category::Category::Photo,
                    container_chain: None,
                },
                100,
            )
            .unwrap();
        }

        let v = get_json_state(state, "/api/copies?hash=DUP").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3, "all three copies, not just the mounted ones");

        let offline = arr
            .iter()
            .find(|m| m["volume_label"] == "Offline HDD")
            .expect("the copy on the unplugged drive must still be listed");
        assert_eq!(offline["mounted"], false, "and be marked as unreachable");
        assert!(arr.iter().any(|m| m["mounted"] == true));
    }

    /// Two drives whose paths do not interleave, with far more rows than any one page.
    ///
    /// This is the shape that broke on the real catalogue: Browse fetched a fixed slice of a
    /// path-ordered search and rebuilt the tree from it, so the drive whose paths sorted later --
    /// 4.75 TB of it -- was simply absent from the response and never drawn. Sizes were wrong for
    /// the same reason: the drive that *did* appear was totalled from the loaded rows alone, showing
    /// 114.6 GB against a real 1.87 TB.
    fn seed_two_drives_that_do_not_interleave() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            for (vid, label) in [("vol-early", "Sorts First"), ("vol-late", "Sorts Last")] {
                cat.upsert_volume(&crate::catalog::models::Volume {
                    volume_id: vid.into(),
                    label: label.into(),
                    identified_by: "marker".into(),
                    first_seen_at: 1,
                    last_seen_at: 1,
                })
                .unwrap();
            }
            // Every path on vol-early sorts before every path on vol-late.
            for (vid, top, n, size) in [
                ("vol-early", "aaa", 60usize, 7i64),
                ("vol-late", "zzz", 5, 11),
            ] {
                for i in 0..n {
                    cat.upsert_file(
                        &crate::catalog::models::NewFile {
                            volume_id: vid.into(),
                            relative_path: format!("{top}/{i:04}/f.txt"),
                            filename: "f.txt".into(),
                            extension: "txt".into(),
                            size_bytes: size,
                            content_hash: format!("{vid}-{i}"),
                            created_time: Some(1),
                            modified_time: Some(1),
                            accessed_time: None,
                            category: crate::category::Category::Document,
                            container_chain: None,
                        },
                        100,
                    )
                    .unwrap();
                }
                cat.rebuild_directory_trees(vid, 200).unwrap();
                cat.refresh_volume_totals(vid).unwrap();
            }
        }
        (tmp, db)
    }

    #[tokio::test]
    async fn every_drive_is_listed_however_its_paths_sort() {
        let (_t, db) = seed_two_drives_that_do_not_interleave();
        let v = get_json(&db, "/api/volumes").await;
        let labels: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["label"].as_str().unwrap())
            .collect();
        assert!(
            labels.contains(&"Sorts First") && labels.contains(&"Sorts Last"),
            "both drives must be offered, got {labels:?}"
        );
    }

    #[tokio::test]
    async fn a_drives_total_is_its_own_not_the_rows_that_happened_to_load() {
        let (_t, db) = seed_two_drives_that_do_not_interleave();
        let v = get_json(&db, "/api/volumes").await;
        let late = v
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["label"] == "Sorts Last")
            .expect("the later-sorting drive must be present");
        assert_eq!(late["active_files"], 5);
        assert_eq!(late["active_bytes"], 55, "5 files x 11 bytes");
    }

    /// A folder page is bounded, and says so, instead of silently truncating the tree.
    #[tokio::test]
    async fn a_folder_page_is_bounded_and_reports_that_there_is_more() {
        let (_t, db) = seed_two_drives_that_do_not_interleave();
        let v = get_json(&db, "/api/folder?volume=vol-early&path=aaa&limit=10").await;
        assert_eq!(v["dirs"].as_array().unwrap().len(), 10);
        assert_eq!(
            v["more_dirs"], true,
            "60 subfolders exist, 10 were returned"
        );

        let rest = get_json(
            &db,
            "/api/folder?volume=vol-early&path=aaa&limit=100&offset=10",
        )
        .await;
        assert_eq!(rest["dirs"].as_array().unwrap().len(), 50);
        assert_eq!(rest["more_dirs"], false);
    }

    /// The later-sorting drive is reachable on its own terms, with real totals.
    #[tokio::test]
    async fn the_later_sorting_drive_can_be_expanded_with_true_totals() {
        let (_t, db) = seed_two_drives_that_do_not_interleave();
        let v = get_json(&db, "/api/folder?volume=vol-late&path=").await;
        let dirs = v["dirs"].as_array().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0]["name"], "zzz");
        assert_eq!(dirs[0]["file_count"], 5);
        assert_eq!(
            dirs[0]["total_bytes"], 55,
            "the folder's whole subtree, not a loaded slice"
        );
    }

    #[tokio::test]
    async fn api_copies_of_an_unknown_hash_is_empty_not_an_error() {
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/copies?hash=nothing-has-this").await;
        assert!(v.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_search_size_bounds_are_inclusive() {
        let (_t, db) = seed_catalog(); // thesis.pdf is 123 B; the archived inner.jpg is 9 B
        let all = get_json(&db, "/api/search").await;
        assert_eq!(all.as_array().unwrap().len(), 2);

        let big = get_json(&db, "/api/search?min_size=123").await;
        let arr = big.as_array().unwrap();
        assert_eq!(arr.len(), 1, "min_size is inclusive of the boundary");
        assert_eq!(arr[0]["filename"], "thesis.pdf");

        let small = get_json(&db, "/api/search?max_size=9").await;
        let arr = small.as_array().unwrap();
        assert_eq!(arr.len(), 1, "max_size is inclusive of the boundary");
        assert_eq!(arr[0]["filename"], "inner.jpg");

        assert!(
            get_json(&db, "/api/search?min_size=10&max_size=100")
                .await
                .as_array()
                .unwrap()
                .is_empty(),
            "a window matching nothing returns nothing, not everything"
        );
    }

    #[tokio::test]
    async fn api_search_date_bounds_exclude_dateless_archive_entries() {
        // thesis.pdf has modified_time=50; the archived inner.jpg has none. A date bound must not
        // silently admit rows with no date -- the Browse UI warns about exactly this.
        let (_t, db) = seed_catalog();
        let after = get_json(&db, "/api/search?modified_after=50").await;
        let arr = after.as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "modified_after is inclusive; NULL is excluded"
        );
        assert_eq!(arr[0]["filename"], "thesis.pdf");

        let before = get_json(&db, "/api/search?modified_before=50").await;
        assert_eq!(before.as_array().unwrap().len(), 1);

        assert!(get_json(&db, "/api/search?modified_after=51")
            .await
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn api_search_copies_null_for_unique_file() {
        let (_t, db) = seed_catalog(); // thesis.pdf (h1) + archived inner.jpg (h2): both unique
        let v = get_json(&db, "/api/search?q=thesis").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["volume_label"], "Test HDD");
        assert!(arr[0]["copies"].is_null());
    }

    #[tokio::test]
    async fn api_volumes_lists_the_volume() {
        let (_t, db) = seed_catalog();
        let v = get_json(&db, "/api/volumes").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["label"], "Test HDD");
    }

    #[tokio::test]
    async fn api_stats_returns_shape() {
        let (_t, db) = seed_catalog();
        let v = get_json(&db, "/api/stats").await;
        assert!(v["duplicate_groups"].is_number());
        assert_eq!(v["volumes"][0]["label"], "Test HDD");
    }

    use std::collections::HashMap;

    // Seed a catalog with a duplicate pair of LOOSE files on one volume, plus a fake mounted drive.
    fn seed_dupes() -> (tempfile::TempDir, PathBuf, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("driveA");
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "Photos HDD".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let mk = |path: &str, created: i64| crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "jpg".into(),
                size_bytes: 10,
                content_hash: "DUP".into(),
                created_time: Some(created),
                modified_time: Some(created),
                accessed_time: None,
                category: crate::category::Category::Photo,
                container_chain: None,
            };
            cat.upsert_file(&mk("a.jpg", 1000), 100).unwrap();
            cat.upsert_file(&mk("copy/a.jpg", 2000), 100).unwrap();
        }
        let mut mounts = HashMap::new();
        mounts.insert("vol-1".to_string(), drive);
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(mounts.clone()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same resolver the rest of the state uses. Handing the queue an empty one would make
            // every job fail with "drive not connected" while the request path looked fine.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(mounts),
            ),
        };
        (tmp, db, state)
    }

    async fn get_json_state(state: AppState, uri: &str) -> serde_json::Value {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK, "uri {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), 5_000_000)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn api_drives_lists_catalogued_volume_with_reclaimable() {
        let (_t, _db, state) = seed_dupes(); // seeds vol-1 "Photos HDD" with a duplicate group
        let v = get_json_state(state, "/api/drives").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["label"], "Photos HDD");
        assert_eq!(arr[0]["connected"], true); // Fixed mount is present
        assert!(arr[0]["reclaimable_bytes"].as_i64().unwrap() >= 0);
    }

    #[tokio::test]
    async fn volume_errors_endpoint_reports_buckets_and_filters() {
        let (_t, db, _state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            cat.log_scan_error(Some("vol-1"), "gone.jpg", "read: i/o", "read", "io", 10)
                .unwrap();
            cat.log_scan_error(
                Some("vol-1"),
                "sysvol",
                "walk: denied",
                "walk",
                "permission",
                10,
            )
            .unwrap();
        }
        let v = get_json(&db, "/api/volumes/vol-1/errors").await;
        assert_eq!(v["totals"]["absent"], 1);
        assert_eq!(v["totals"]["unreadable_dirs"], 1);
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);

        let only = get_json(&db, "/api/volumes/vol-1/errors?bucket=unreadable_dir").await;
        let rows = only["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"], "sysvol");
        assert_eq!(rows[0]["kind"], "permission");
    }

    #[tokio::test]
    async fn a_clean_volume_reports_complete() {
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/volumes/vol-1/errors").await;
        assert_eq!(v["totals"]["absent"], 0);
        assert_eq!(v["totals"]["unverified"], 0);
        assert_eq!(v["totals"]["unreadable_dirs"], 0);
        assert!(v["rows"].as_array().unwrap().is_empty());
    }

    /// End-to-end of the review flow's visibility rules (the scenario reported by the user):
    /// quarantining a duplicate keeps the drive's "Purge" affordance alive (quarantined_bytes),
    /// even though reclaimable-from-duplicates has dropped to 0; the quarantined row is browsable at
    /// its original location; and once purged it is hidden by default but still reachable (at its
    /// original location) via the Purged status filter. Status counts flag each kind's presence.
    #[tokio::test]
    async fn quarantine_then_purge_visibility_and_purge_affordance() {
        let (_t, db, _state) = seed_dupes(); // two active copies of hash DUP: a.jpg + copy/a.jpg (10 B each)

        // Quarantine copy/a.jpg -> moves under _ToDelete, remembers its original location.
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            let id = cat.loose_file_id("vol-1", "copy/a.jpg").unwrap().unwrap();
            cat.mark_quarantined(id, "_ToDelete/copy/a.jpg", "copy/a.jpg", 300)
                .unwrap();
        }

        // Purge affordance: driven by quarantined_bytes (10), NOT reclaimable_bytes (now 0 — the
        // duplicate group is gone). This is exactly the "purge button disappeared" regression.
        let d = get_json(&db, "/api/drives").await;
        let d0 = &d.as_array().unwrap()[0];
        assert_eq!(d0["quarantined_bytes"], 10);
        assert_eq!(d0["reclaimable_bytes"], 0);

        // Status filter flags the kinds that are present.
        let counts = get_json(&db, "/api/status-counts").await;
        assert_eq!(counts["active"], 1);
        assert_eq!(counts["quarantined"], 1);

        // Default browse shows the quarantined row at its original location, never under _ToDelete.
        let hits = get_json(&db, "/api/search").await;
        let q = hits
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["status"] == "quarantined")
            .expect("quarantined row is browsable");
        assert_eq!(q["original_path"], "copy/a.jpg");
        assert_eq!(q["relative_path"], "_ToDelete/copy/a.jpg");

        // Purge it.
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            let id = cat
                .loose_file_id("vol-1", "_ToDelete/copy/a.jpg")
                .unwrap()
                .unwrap();
            cat.mark_purged(id, 400).unwrap();
        }

        // Hidden by default, still surfaced (at its original location) by the Purged filter.
        let after = get_json(&db, "/api/search").await;
        assert!(
            after
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["status"] != "purged"),
            "purged rows are hidden from the default tree"
        );
        let purged = get_json(&db, "/api/search?status=purged").await;
        let parr = purged.as_array().unwrap();
        assert_eq!(parr.len(), 1);
        assert_eq!(parr[0]["original_path"], "copy/a.jpg");

        // Nothing left in quarantine; the Purged kind is now flagged.
        let d2 = get_json(&db, "/api/drives").await;
        assert_eq!(d2.as_array().unwrap()[0]["quarantined_bytes"], 0);
        let counts2 = get_json(&db, "/api/status-counts").await;
        assert_eq!(counts2["purged"], 1);
    }

    #[tokio::test]
    async fn api_search_multi_status_filter_ors_values() {
        let (_t, db, _state) = seed_dupes(); // a.jpg + copy/a.jpg, both active
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            let id = cat.loose_file_id("vol-1", "copy/a.jpg").unwrap().unwrap();
            cat.mark_quarantined(id, "_ToDelete/copy/a.jpg", "copy/a.jpg", 300)
                .unwrap();
        }
        // single value
        let a = get_json(&db, "/api/search?status=active").await;
        assert_eq!(a.as_array().unwrap().len(), 1);
        let qd = get_json(&db, "/api/search?status=quarantined").await;
        assert_eq!(qd.as_array().unwrap().len(), 1);
        // multi value: comma-joined statuses are OR-combined
        let both = get_json(&db, "/api/search?status=active,quarantined").await;
        assert_eq!(both.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_duplicates_groups_with_suggested_keep_and_mounted() {
        let (_t, _db, state) = seed_dupes();
        // The fixture's files are 10 B, so the 1 MiB default floor would (correctly) hide them.
        let v = get_json_state(state, "/api/duplicates?min_size=0").await;
        let arr = v["groups"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["members"].as_array().unwrap().len(), 2);
        // earliest created_time (1000) is a.jpg -> its id is the suggested keep
        let members = arr[0]["members"].as_array().unwrap();
        let keep = arr[0]["suggested_keep_id"].as_i64().unwrap();
        let a = members.iter().find(|m| m["filename"] == "a.jpg").unwrap();
        assert_eq!(a["id"].as_i64().unwrap(), keep);
        assert_eq!(a["volume_label"], "Photos HDD");
        assert_eq!(a["mounted"], true);
        assert_eq!(a["is_loose"], true);
        // one redundant 10 B copy
        assert_eq!(arr[0]["reclaimable_bytes"].as_i64().unwrap(), 10);
    }

    #[tokio::test]
    async fn api_duplicates_is_ranked_bounded_and_reports_totals() {
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/duplicates?min_size=0&limit=1").await;
        assert!(v["totals"]["reclaimable_all_bytes"].as_i64().unwrap() > 0);
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "limit must bound the page");
        assert!(groups[0]["suggested_keep_id"].as_i64().unwrap() > 0);
        assert!(!groups[0]["members"].as_array().unwrap().is_empty());
        assert!(
            v["next"]["content_hash"].is_string(),
            "a cursor for the next page"
        );
    }

    #[tokio::test]
    async fn api_duplicates_omits_totals_on_continuation_pages() {
        let (_t, db, _state) = seed_dupes();
        let first = get_json(&db, "/api/duplicates?min_size=0").await;
        assert!(first["totals"].is_object(), "first page carries the totals");
        let n = &first["next"];
        let uri = format!(
            "/api/duplicates?min_size=0&after_reclaimable={}&after_hash={}",
            n["reclaimable_bytes"].as_i64().unwrap(),
            n["content_hash"].as_str().unwrap()
        );
        let page2 = get_json(&db, &uri).await;
        assert!(
            page2["totals"].is_null(),
            "continuation pages must not re-pay for three full aggregate passes"
        );
    }

    #[tokio::test]
    async fn api_duplicates_floor_filters_the_list_but_not_the_headline() {
        let (_t, db, _state) = seed_dupes();
        let all = get_json(&db, "/api/duplicates?min_size=0").await;
        let floored = get_json(&db, "/api/duplicates?min_size=999999999").await;
        assert!(
            floored["groups"].as_array().unwrap().is_empty(),
            "the floor empties the list"
        );
        assert_eq!(
            floored["totals"]["reclaimable_all_bytes"], all["totals"]["reclaimable_all_bytes"],
            "the headline must not move when the floor changes"
        );
        assert_eq!(
            floored["totals"]["groups_all"], all["totals"]["groups_all"],
            "the headline must not move when the floor changes"
        );
    }

    fn tiny_png() -> Vec<u8> {
        // 2x2 red PNG, generated via the image crate.
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn preview_returns_jpeg_for_loose_photo_on_mounted_drive() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, db, state) = seed_dupes();
        // write a real image at the loose path on the fake drive
        let drive = match &state.mounts {
            crate::mounts::MountResolver::Fixed(m) => m["vol-1"].clone(),
            _ => unreachable!(),
        };
        std::fs::write(drive.join("a.jpg"), tiny_png()).unwrap();
        // find a.jpg's id
        let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
        let id = cat.loose_file_id("vol-1", "a.jpg").unwrap().unwrap();

        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/preview/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let ct = res
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(ct, "image/jpeg");
        let bytes = axum::body::to_bytes(res.into_body(), 5_000_000)
            .await
            .unwrap();
        assert!(image::load_from_memory(&bytes).is_ok());
    }

    #[tokio::test]
    async fn preview_returns_404_for_undecodable_bytes() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, db, state) = seed_dupes();
        // write garbage bytes (not an image) at the loose path on the fake drive
        let drive = match &state.mounts {
            crate::mounts::MountResolver::Fixed(m) => m["vol-1"].clone(),
            _ => unreachable!(),
        };
        std::fs::write(drive.join("a.jpg"), b"this is not an image").unwrap();
        // find a.jpg's id
        let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
        let id = cat.loose_file_id("vol-1", "a.jpg").unwrap().unwrap();

        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/preview/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn preview_returns_404_for_non_photo() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, db, state) = seed_dupes();
        // insert a DOCUMENT-category loose file into the catalog
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            let doc = crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: "notes.txt".into(),
                filename: "notes.txt".into(),
                extension: "txt".into(),
                size_bytes: 100,
                content_hash: "doc_hash".into(),
                created_time: Some(3000),
                modified_time: Some(3000),
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&doc, 100).unwrap();
        }
        // find notes.txt's id
        let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
        let id = cat.loose_file_id("vol-1", "notes.txt").unwrap().unwrap();

        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/preview/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    }

    async fn post_json(
        state: AppState,
        uri: &str,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("x-cleanup-token", t);
        }
        let app = build_router_with(state);
        let res = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 5_000_000)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn forget_drive_requires_token_then_removes() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(
            state.clone(),
            "/api/forget-drive",
            None,
            serde_json::json!({"volume_id":"vol-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, json) = post_json(
            state,
            "/api/forget-drive",
            Some("T"),
            serde_json::json!({"volume_id":"vol-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["removed_files"].as_i64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn rename_drive_requires_token_then_persists_effective_name() {
        let (_t, db, state) = seed_dupes();
        let (status, _) = post_json(
            state.clone(),
            "/api/rename-drive",
            None,
            serde_json::json!({"volume_id":"vol-1","name":"Trip 2019"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, json) = post_json(
            state,
            "/api/rename-drive",
            Some("T"),
            serde_json::json!({"volume_id":"vol-1","name":"Trip 2019","description":"summer"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["name"], "Trip 2019");
        // effective name now flows into /api/search and /api/drives
        let v = get_json(&db, "/api/search").await;
        assert_eq!(v.as_array().unwrap()[0]["volume_label"], "Trip 2019");
        let d = get_json(&db, "/api/drives").await;
        assert_eq!(d.as_array().unwrap()[0]["display_name"], "Trip 2019");
        assert_eq!(d.as_array().unwrap()[0]["description"], "summer");
    }

    #[tokio::test]
    async fn purge_all_requires_token() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(state, "/api/purge-all", None, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// The CSRF token `seed_dupes`'s `AppState` is fixed to; named so new tests don't have to
    /// guess (or drift from) the literal every write-endpoint test already passes.
    const TEST_TOKEN: &str = "T";

    #[tokio::test]
    async fn scan_stop_requires_csrf_and_answers_when_idle() {
        let (_t, _db, state) = seed_dupes();
        // Without a token the write endpoint must refuse, like every other write endpoint.
        let app = build_router_with(state.clone());
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/scan/stop")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);

        // With a token, stopping while nothing runs is a no-op success, not an error.
        let (code, v) = post_json(
            state,
            "/api/scan/stop",
            Some(TEST_TOKEN),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK);
        assert_eq!(v["stopping"], false, "nothing was running");
    }

    #[tokio::test]
    async fn quarantine_requires_csrf_token() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(
            state,
            "/api/quarantine",
            None,
            serde_json::json!({"quarantine_ids":[1]}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn quarantine_is_queued_and_the_worker_completes_it() {
        // The POST now ENQUEUES rather than doing the work, so the reviewer can confirm the next
        // group immediately instead of waiting out a re-hash (#66 for folders, this for files).
        let (_t, db, state) = seed_dupes();
        let drive = match &state.mounts {
            crate::mounts::MountResolver::Fixed(m) => m["vol-1"].clone(),
            _ => unreachable!(),
        };
        std::fs::create_dir_all(drive.join("copy")).unwrap();
        std::fs::write(drive.join("a.jpg"), b"DUP").unwrap();
        std::fs::write(drive.join("copy/a.jpg"), b"DUP").unwrap();
        let victim = {
            let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
            cat.loose_file_id("vol-1", "copy/a.jpg").unwrap().unwrap()
        };
        tokio::spawn(state.quarantine_queue.clone().run_worker());

        let (status, json) = post_json(
            state.clone(),
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [victim] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["queued"], 1, "one id accepted");
        assert_eq!(json["position"], 0, "nothing ahead of it");

        for _ in 0..400 {
            if drive.join("_ToDelete/copy/a.jpg").is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            drive.join("_ToDelete/copy/a.jpg").is_file(),
            "the queued move must happen"
        );
        assert!(!drive.join("copy/a.jpg").exists());
        assert!(drive.join("a.jpg").exists(), "survivor stays");
    }

    #[tokio::test]
    async fn quarantine_reports_an_unmounted_volume_without_queueing_it() {
        // Whether a drive is plugged in IS knowable at request time, so the reviewer learns it from
        // the response rather than from a poll thirty seconds later.
        let (_t, db, state) = seed_dupes();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-2".into(),
                label: "Unplugged".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            cat.upsert_file(
                &crate::catalog::models::NewFile {
                    volume_id: "vol-2".into(),
                    relative_path: "x.jpg".into(),
                    filename: "x.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 3,
                    content_hash: "DUPHASH".into(),
                    created_time: None,
                    modified_time: None,
                    accessed_time: None,
                    category: crate::category::Category::Photo,
                    container_chain: None,
                },
                100,
            )
            .unwrap();
        }
        let id = {
            let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
            cat.loose_file_id("vol-2", "x.jpg").unwrap().unwrap()
        };
        let (status, json) = post_json(
            state,
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [id] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["queued"], 0, "nothing could be queued");
        assert!(json["unmounted_volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "vol-2"));
    }

    #[tokio::test]
    async fn quarantine_ids_that_do_not_resolve_are_reported_skipped() {
        let (_t, _db, state) = seed_dupes();
        let (status, json) = post_json(
            state,
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [999_999] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["skipped"], 1);
        assert_eq!(json["queued"], 0);
    }

    #[tokio::test]
    async fn repack_requires_csrf_token() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(
            state,
            "/api/repack",
            None,
            serde_json::json!({"entry_id": 1}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn repack_removes_entry_over_http() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("driveA");
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
        {
            use std::io::Write;
            let f = std::fs::File::create(drive.join("bundle.zip")).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (n, b) in [("keep.txt", &b"KEEP"[..]), ("dup.txt", &b"SHARED"[..])] {
                zw.start_file(n, opts).unwrap();
                zw.write_all(b).unwrap();
            }
            zw.finish().unwrap();
        }
        std::fs::write(drive.join("loose_dup.txt"), b"SHARED").unwrap();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let ident = crate::volume::VolumeIdentity {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
            };
            crate::scanner::scan_volume(&cat, &drive, &ident, false, 100, &test_limits()).unwrap();
        }
        let mut mounts = std::collections::HashMap::new();
        mounts.insert("vol-1".to_string(), drive.clone());
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(mounts.clone()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same resolver the rest of the state uses. Handing the queue an empty one would make
            // every job fail with "drive not connected" while the request path looked fine.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(mounts),
            ),
        };

        let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
        let entry_id = cat
            .archive_entries("vol-1", "bundle.zip")
            .unwrap()
            .into_iter()
            .find(|e| e.container_chain.as_deref() == Some("dup.txt"))
            .unwrap()
            .id;
        drop(cat);

        let (status, json) = post_json(
            state,
            "/api/repack",
            Some("T"),
            serde_json::json!({"entry_id": entry_id}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "body {json}");
        assert_eq!(json["removed_entry"], "dup.txt");

        let f = std::fs::File::open(drive.join("bundle.zip")).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        assert!(z.by_name("keep.txt").is_ok());
        assert!(z.by_name("dup.txt").is_err());
    }

    #[tokio::test]
    async fn repack_returns_409_when_drive_not_connected() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let entry_id;
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            cat.upsert_archive_entry(
                "vol-1",
                "bundle.zip",
                &crate::archive::ArchiveEntry {
                    container_chain: "inner.txt".into(),
                    filename: "inner.txt".into(),
                    extension: "txt".into(),
                    size_bytes: 6,
                    content_hash: "H".into(),
                },
                None,
                100,
            )
            .unwrap();
            entry_id = cat
                .archive_entries("vol-1", "bundle.zip")
                .unwrap()
                .into_iter()
                .find(|e| e.container_chain.as_deref() == Some("inner.txt"))
                .unwrap()
                .id;
        }
        // No volumes mounted at all.
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(std::collections::HashMap::new()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same (empty) resolver the rest of this state uses: these fixtures deliberately mount
            // nothing, and the queue must agree with the request path about that.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(std::collections::HashMap::new()),
            ),
        };

        let (status, _) = post_json(
            state,
            "/api/repack",
            Some("T"),
            serde_json::json!({"entry_id": entry_id}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn repack_returns_400_when_no_survivor() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("driveA");
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
        {
            use std::io::Write;
            let f = std::fs::File::create(drive.join("bundle.zip")).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (n, b) in [("keep.txt", &b"KEEP"[..]), ("dup.txt", &b"SHARED"[..])] {
                zw.start_file(n, opts).unwrap();
                zw.write_all(b).unwrap();
            }
            zw.finish().unwrap();
        }
        // NOTE: no loose survivor copy written this time — dup.txt inside the zip is the only copy.
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let ident = crate::volume::VolumeIdentity {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
            };
            crate::scanner::scan_volume(&cat, &drive, &ident, false, 100, &test_limits()).unwrap();
        }
        let mut mounts = std::collections::HashMap::new();
        mounts.insert("vol-1".to_string(), drive.clone());
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(mounts.clone()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same resolver the rest of the state uses. Handing the queue an empty one would make
            // every job fail with "drive not connected" while the request path looked fine.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(mounts),
            ),
        };

        let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
        let entry_id = cat
            .archive_entries("vol-1", "bundle.zip")
            .unwrap()
            .into_iter()
            .find(|e| e.container_chain.as_deref() == Some("dup.txt"))
            .unwrap()
            .id;
        drop(cat);

        let (status, _json) = post_json(
            state,
            "/api/repack",
            Some("T"),
            serde_json::json!({"entry_id": entry_id}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

        // Archive untouched: dup.txt is still inside.
        let f = std::fs::File::open(drive.join("bundle.zip")).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        assert!(z.by_name("dup.txt").is_ok());
    }

    #[tokio::test]
    async fn pick_folder_requires_csrf_token() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(state, "/api/pick-folder", None, serde_json::json!({})).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn scan_requires_csrf_token() {
        let (_t, _db, state) = seed_dupes();
        let (status, _) = post_json(
            state,
            "/api/scan",
            None,
            serde_json::json!({"path":"whatever","force":false}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn scan_enqueues_and_status_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("drive");
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join("a.txt"), b"hi").unwrap();
        {
            crate::catalog::Catalog::open(&db).unwrap();
        }
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(std::collections::HashMap::new()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same (empty) resolver the rest of this state uses: these fixtures deliberately mount
            // nothing, and the queue must agree with the request path about that.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(std::collections::HashMap::new()),
            ),
        };
        // must run the worker for the enqueued job to progress
        tokio::spawn(state.scan_queue.clone().run_worker());
        tokio::spawn(state.quarantine_queue.clone().run_worker());

        let (status, json) = post_json(
            state.clone(),
            "/api/scan",
            Some("T"),
            serde_json::json!({"path": drive.to_string_lossy(), "force": false}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "body {json}");

        // poll status until the scan finishes
        let done = {
            let mut found = false;
            for _ in 0..200 {
                let v = get_json_state(state.clone(), "/api/scan/status").await;
                if v["recent"]
                    .as_array()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
                {
                    found = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            found
        };
        assert!(done, "scan should have completed and appeared in recent");
    }

    #[tokio::test]
    async fn review_page_is_self_contained_and_has_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/review")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("name=\"csrf\""), "token meta present");
        assert!(body.contains("/api/duplicates"), "fetches duplicates");
        assert!(body.contains("/api/quarantine"), "posts to quarantine");
        assert!(
            !body.contains("http://") && !body.contains("https://"),
            "self-contained"
        );
    }

    async fn get_text(state: AppState, uri: &str) -> String {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 10_000_000)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn review_page_confirms_without_promising_a_wait() {
        // The old copy said "Verifying content, then quarantining… (large files take a while)" —
        // true when the request blocked, a lie now that it queues.
        let (_t, _db, state) = seed_dupes();
        let body = get_text(state, "/review").await;
        assert!(
            !body.contains("Verifying content"),
            "the blocking wording must be gone"
        );
        assert!(
            body.contains("Queued"),
            "the reviewer is told it was queued"
        );
        assert!(
            body.contains("st.running.label"),
            "the poller reads label, not the removed path field"
        );
    }

    #[tokio::test]
    async fn confirm_button_is_disabled_for_the_duration_of_the_request() {
        // F1: a double-click on Confirm used to run `idx++; render();` twice -- the server-side
        // de-dup stops the file being queued twice, but nothing stopped the reviewer being
        // advanced past a group they never saw. Disabling the button around the await (and
        // re-enabling in a `finally`, so a thrown request cannot leave it dead forever) is what
        // makes the second click a no-op instead of a second advance.
        let (_t, _db, state) = seed_dupes();
        let body = get_text(state, "/review").await;
        let handler_start = body
            .find("$(\"#confirm\").addEventListener")
            .expect("confirm click handler must exist");
        let handler_end = body[handler_start..]
            .find("$(\"#skip\").addEventListener")
            .map(|i| handler_start + i)
            .expect("skip handler follows confirm handler");
        let handler = &body[handler_start..handler_end];
        assert!(
            handler.contains("$(\"#confirm\").disabled=true"),
            "the button must be disabled before the request is sent"
        );
        assert!(
            handler.contains("finally"),
            "re-enabling must happen in a finally so a thrown request can't leave it dead"
        );
        assert!(
            handler.contains("$(\"#confirm\").disabled=false"),
            "the button must be re-enabled once the request settles"
        );
    }

    #[tokio::test]
    async fn skipped_wording_does_not_claim_a_cause_it_cannot_back_up() {
        // F3: `quarantine_files` increments `skipped` for five different reasons -- only one of
        // them is the last-copy guard. Wording it as "kept by the last-copy guard" reports an I/O
        // error on the user's drive as the safety guard working correctly, which is the wrong
        // direction to be wrong in.
        let (_t, _db, state) = seed_dupes();
        let body = get_text(state, "/review").await;
        assert!(
            !body.contains("last-copy guard"),
            "must not claim the last-copy guard specifically -- skipped has four other causes"
        );
        assert!(
            body.contains("kept (not moved"),
            "must use wording that does not assert a cause"
        );
    }

    #[tokio::test]
    async fn index_page_has_search_ui_and_calls_api() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(PathBuf::from("unused.db"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/browse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("id=\"q\""), "search input present");
        assert!(body.contains("id=\"results\""), "results container present");
        assert!(body.contains("/api/search"), "page fetches the search API");
        assert!(
            body.contains("buildTree") && body.contains("renderTree"),
            "renders a tree"
        );
        assert!(body.contains("class=\"tree\""), "tree container present");
        // self-contained: no external resource references
        assert!(!body.contains("http://"), "no external http resources");
        assert!(!body.contains("https://"), "no external https resources");
    }

    #[tokio::test]
    async fn root_is_overview_and_self_contained() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("/api/activity"), "overview loads activity");
        assert!(body.contains("/api/drives"), "overview loads drives");
        assert!(body.contains("Recent activity"));
        assert!(
            !body.contains("http://") && !body.contains("https://"),
            "self-contained"
        );
    }

    #[tokio::test]
    async fn shell_has_theme_toggle() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("data-theme=\"dark\"") && body.contains("themebar"),
            "theme toggle present"
        );
        assert!(body.contains("applyTheme"), "theme JS present");
        assert!(
            !body.contains("http://") && !body.contains("https://"),
            "self-contained"
        );
    }

    #[tokio::test]
    async fn detected_drives_flags_catalogued() {
        let (_t, _db, state) = seed_dupes(); // Fixed mount vol-1 -> driveA (marker vol-1), catalogued
        let v = get_json_state(state, "/api/detected-drives").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["catalogued"], true);
        assert_eq!(arr[0]["volume_label"], "Photos HDD");
    }

    #[tokio::test]
    async fn scan_page_is_self_contained_and_wired() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri("/scan").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("name=\"csrf\""));
        assert!(body.contains("/api/scan"));
        assert!(body.contains("/api/detected-drives"));
        assert!(body.contains("/api/pick-folder"));
        // The Stop button is the only way to end a scan from the browser; a page that renders
        // without it leaves a multi-day scan unstoppable short of killing the process.
        assert!(body.contains("/api/scan/stop"));
        assert!(body.contains("stopscan"));
        // The limits are only reachable from here; a page without the section leaves the user
        // editing JSON by hand, which is what this feature exists to avoid.
        assert!(body.contains("/api/settings"));
        assert!(body.contains("archive_ratio_cap"));
        assert!(body.contains("/api/pending-formats"));
        assert!(body.contains("humanBytes"), "byte fields carry a unit hint");
        // The extension lists are only reachable from here; without them a user who mis-clicks
        // "Treat as documents" cannot see it happened or undo it except by hand-editing settings.json.
        assert!(body.contains("archive_deny_extensions"));
        assert!(body.contains("archive_allow_extensions"));
        assert!(!body.contains("http://") && !body.contains("https://"));
    }

    #[tokio::test]
    async fn drives_page_is_wired_and_self_contained() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/drives")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("name=\"csrf\""));
        assert!(body.contains("/api/drives"));
        assert!(body.contains("/api/forget-drive"));
        assert!(body.contains("/api/purge-all"));
        assert!(body.contains("/api/rename-drive"), "drives page can rename");
        // The completeness panel is the only way to see WHICH files are missing; a page without it
        // leaves that answer unreachable from the browser.
        assert!(body.contains("/api/volumes/"));
        assert!(body.contains("completeness"));
        assert!(!body.contains("http://") && !body.contains("https://"));
    }

    #[tokio::test]
    async fn console_page_is_self_contained_and_maps_commands() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/console")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 2_000_000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("name=\"csrf\""));
        assert!(
            body.contains("/api/stats")
                && body.contains("/api/scan")
                && body.contains("/api/purge-all")
        );
        assert!(
            !body.contains("http://") && !body.contains("https://"),
            "self-contained"
        );
    }

    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    #[allow(
        clippy::await_holding_lock,
        reason = "the lock is a test-only serialization guard (unit-value Mutex), not a \
            resource read across the await; holding it for the whole async test body is the point"
    )]
    async fn request_is_traced_with_method_status_and_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        // Serialize with other subscriber-installing tests (tracing's interest cache is global).
        let _tracing_lock = crate::observability::tracing_test_guard();
        let (_t, db) = seed_catalog();
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .with_writer(CaptureWriter(buf.clone()))
            .with_ansi(false) // the custom writer isn't a terminal; disable ANSI so "id=" etc. are contiguous
            .finish();
        let _guard = tracing::subscriber::set_default(sub); // held across the await (current-thread test)

        let app = build_router(db.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/search?q=thesis")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("GET"), "log: {logged}");
        assert!(logged.contains("200"), "log: {logged}");
        assert!(logged.contains("id="), "request-id field present: {logged}");
    }

    #[tokio::test]
    #[allow(
        clippy::await_holding_lock,
        reason = "the lock is a test-only serialization guard (unit-value Mutex), not a \
            resource read across the await; holding it for the whole async test body is the point"
    )]
    async fn csrf_rejection_is_logged() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        // Serialize with other subscriber-installing tests (tracing's interest cache is global).
        let _tracing_lock = crate::observability::tracing_test_guard();
        let (_t, _db, state) = seed_dupes();
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .with_writer(CaptureWriter(buf.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let app = build_router_with(state);
        // POST /api/quarantine with NO token -> 403 and a warn line
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/quarantine")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"quarantine_ids\":[1]}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);

        let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("WARN"), "expected a warn line: {logged}");
        assert!(
            logged.to_lowercase().contains("token"),
            "reason mentions token: {logged}"
        );
    }

    #[tokio::test]
    async fn api_activity_returns_formatted_rows() {
        let (_t, db, state) = seed_dupes();
        {
            // write actions to read back (newest-first: purge@500, then quarantine@400)
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.log_action(
                "purge",
                "{\"volume_id\":\"vol-1\",\"files_purged\":3,\"bytes_reclaimed\":2048}",
                500,
            )
            .unwrap();
            // Real quarantine payload shape from src/quarantine.rs: `from` is the relative path.
            cat.log_action("quarantine",
                "{\"file_id\":1,\"volume_id\":\"vol-1\",\"from\":\"docs/report.txt\",\"to\":\"_ToDelete/report.txt\",\"hash\":\"h\"}", 400).unwrap();
        }
        let v = get_json_state(state, "/api/activity").await;
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["kind"], "purge");
        assert!(arr[0]["summary"].as_str().unwrap().contains("Purged"));
        assert_eq!(arr[0]["occurred_at"], 500);
        // The quarantine feed entry must name the file (basename of `from`), not render blank.
        let q = arr
            .iter()
            .find(|e| e["kind"] == "quarantine")
            .expect("quarantine entry present");
        assert!(
            q["summary"].as_str().unwrap().contains("report.txt"),
            "quarantine summary should name the file: {}",
            q["summary"]
        );
    }

    #[tokio::test]
    async fn api_scan_runs_lists_recent_runs_newest_first() {
        let (_t, db, _state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            let id = cat
                .start_scan_run(Some("vol-1"), "D:/one", 100, false)
                .unwrap();
            let s = crate::scanner::ScanSummary {
                hashed: 3,
                metrics: crate::scan_metrics::MetricsSnapshot {
                    hash_ms: 42,
                    histogram: [0, 1, 2, 0, 0, 0, 0],
                    ..Default::default()
                },
                ..Default::default()
            };
            cat.finish_scan_run(id, 150, "completed", None, &s).unwrap();
            // A current timestamp: this row represents a scan that is still running, and a row
            // stamped 1970 with nothing beating for it is correctly reported interrupted (#36).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            cat.start_scan_run(Some("vol-1"), "D:/two", now, true)
                .unwrap();
        }

        let v = get_json(&db, "/api/scan-runs?limit=10").await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["root_path"], "D:/two", "newest first");
        assert_eq!(arr[0]["status"], "running");
        assert_eq!(arr[1]["status"], "completed");
        assert_eq!(arr[1]["hashed"], 3);
        assert_eq!(arr[1]["metrics"]["hash_ms"], 42);
        assert_eq!(arr[1]["metrics"]["histogram"][2], 2);
    }

    #[tokio::test]
    async fn api_scan_runs_clamps_its_limit() {
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/scan-runs?limit=100000").await;
        assert!(v.is_array(), "an absurd limit must not error: {v}");
    }

    /// `/api/settings` reads and writes via `Config::default_paths()`, which falls through to the
    /// user's real app-data directory unless `CLEANUPSTORAGES_DATA_DIR` is set. This guard points
    /// it at a throwaway tempdir for the test's duration and restores whatever was there before on
    /// drop (even on panic), using the same mutex `config.rs` uses so the two never race on the
    /// process-global env var.
    struct ScopedDataDir {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev: Option<String>,
    }
    impl ScopedDataDir {
        fn new() -> Self {
            let lock = crate::config::ENV_GUARD
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var("CLEANUPSTORAGES_DATA_DIR").ok();
            std::env::set_var("CLEANUPSTORAGES_DATA_DIR", dir.path());
            ScopedDataDir {
                _lock: lock,
                _dir: dir,
                prev,
            }
        }
        /// The directory this guard points CLEANUPSTORAGES_DATA_DIR at, so a test can assert
        /// nothing was written there.
        fn path(&self) -> &std::path::Path {
            self._dir.path()
        }
    }
    impl Drop for ScopedDataDir {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("CLEANUPSTORAGES_DATA_DIR", v),
                None => std::env::remove_var("CLEANUPSTORAGES_DATA_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn settings_endpoint_returns_the_effective_limits() {
        let _scope = ScopedDataDir::new();
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/settings").await;
        assert_eq!(v["archive_ratio_cap"], 10000);
        assert_eq!(v["max_archive_depth"], 8);
    }

    #[tokio::test]
    async fn posting_settings_without_a_csrf_token_is_refused() {
        // Every write endpoint in this file is guarded; a settings write is no exception.
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/settings")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"archive_ratio_cap":5000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_oversized_buffer_budget_is_refused_with_a_reason() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let token = state.csrf_token.clone();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/settings")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", token)
                    .body(Body::from(
                        r#"{"archive_total_buffer_bytes": 1000000000000000}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), 100_000)
            .await
            .unwrap();
        let msg = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            msg.to_lowercase().contains("memory"),
            "the refusal must say why: {msg}"
        );
    }

    /// A `Config` literal standing in for "the config in effect before this write" -- used to drive
    /// `validate` directly, since the real memory ceiling (`sysinfo`) is not mockable and this needs
    /// to hold regardless of how much RAM the test machine actually has.
    fn config_with_buffer(buffer_bytes: u64, total_bytes: u64) -> crate::config::Config {
        crate::config::Config {
            catalog_path: std::path::PathBuf::from("unused/catalog.db"),
            snapshot_retention: 10,
            max_archive_depth: 8,
            archive_buffer_max_bytes: buffer_bytes,
            archive_total_buffer_bytes: total_bytes,
            archive_entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
            archive_ratio_cap: 10_000,
            archive_deny_extensions: Vec::new(),
            archive_allow_extensions: Vec::new(),
        }
    }

    #[test]
    fn f4_zero_is_refused_for_every_byte_limit() {
        // F4: zero converts present archive entries to `missing` on the next scan even though
        // nothing on disk changed (archive_entry_max_bytes: 0 rejects every entry; the buffer
        // fields at 0 buffer nothing). Unlimited is spelled `null`, never `0`.
        let before = config_with_buffer(2 * 1024 * 1024 * 1024, 2 * 1024 * 1024 * 1024);
        let zero_buffer = crate::config::Settings {
            archive_buffer_max_bytes: Some(0),
            ..Default::default()
        };
        assert!(
            validate(&zero_buffer, &before).is_err(),
            "0 must be refused for archive_buffer_max_bytes"
        );

        let zero_total = crate::config::Settings {
            archive_total_buffer_bytes: Some(0),
            ..Default::default()
        };
        assert!(
            validate(&zero_total, &before).is_err(),
            "0 must be refused for archive_total_buffer_bytes"
        );

        let zero_entry = crate::config::Settings {
            archive_entry_max_bytes: Some(Some(0)),
            ..Default::default()
        };
        assert!(
            validate(&zero_entry, &before).is_err(),
            "0 must be refused for archive_entry_max_bytes -- unlimited is null, not 0"
        );

        // Unlimited (null) must still be accepted.
        let unlimited_entry = crate::config::Settings {
            archive_entry_max_bytes: Some(None),
            ..Default::default()
        };
        assert!(
            validate(&unlimited_entry, &before).is_ok(),
            "null (explicitly unlimited) must remain valid"
        );
    }

    #[test]
    fn f7_an_unchanged_oversized_buffer_is_not_refused() {
        // F7: on a machine with under 8 GiB RAM, the compiled-in default archive_buffer_max_bytes
        // (2 GiB) already exceeds RAM/4, so checking every save unconditionally would refuse EVERY
        // save on such a machine -- including one that only edits the ratio cap and resends the
        // buffer fields unchanged. A value larger than any real machine's RAM/4 is used here so the
        // assertion holds regardless of the test machine's actual memory.
        let absurdly_large = u64::MAX / 2;
        let before = config_with_buffer(absurdly_large, absurdly_large);
        let s = crate::config::Settings {
            archive_buffer_max_bytes: Some(absurdly_large),
            archive_total_buffer_bytes: Some(absurdly_large),
            archive_ratio_cap: Some(4321),
            ..Default::default()
        };
        assert!(
            validate(&s, &before).is_ok(),
            "an unchanged (or lowered) value must never be refused by the memory ceiling"
        );
    }

    #[test]
    fn f7_raising_the_buffer_past_the_ceiling_is_still_refused() {
        // The other half of F7: the ceiling must still bite when a value is genuinely being raised.
        let before = config_with_buffer(1, 1);
        let s = crate::config::Settings {
            archive_buffer_max_bytes: Some(u64::MAX / 2),
            ..Default::default()
        };
        assert!(
            validate(&s, &before).is_err(),
            "raising a value past the memory ceiling must still be refused"
        );
    }

    /// POST `/api/settings` with the CSRF token attached, returning status and the raw body text
    /// (a success body is JSON, a rejection body is plain text -- callers parse only what they need).
    async fn post_settings(
        state: AppState,
        token: &str,
        body: &str,
    ) -> (axum::http::StatusCode, String) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/settings")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", token)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 100_000)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_partial_post_does_not_reset_the_fields_it_omits() {
        // The reviewer's scenario: set one field, then POST only an unrelated one. The first
        // field must survive -- Task 4's whole point was that every field is optional, so a
        // partial write must not silently reset the rest to their compiled-in defaults.
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let token = state.csrf_token.clone();

        let (status, _) =
            post_settings(state.clone(), &token, r#"{"archive_ratio_cap": 9999}"#).await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let (status, _) = post_settings(state.clone(), &token, r#"{"max_archive_depth": 3}"#).await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let v = get_json_state(state, "/api/settings").await;
        assert_eq!(
            v["archive_ratio_cap"], 9999,
            "an earlier, unrelated field must survive a later partial POST"
        );
        assert_eq!(v["max_archive_depth"], 3);
    }

    #[tokio::test]
    async fn an_explicit_null_still_sets_unlimited_not_omission() {
        // `null` and "key absent" both look like Rust's `None`, but they must mean different
        // things: absent keeps whatever is stored, null explicitly overwrites it to unlimited.
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let token = state.csrf_token.clone();

        let (status, _) = post_settings(
            state.clone(),
            &token,
            r#"{"archive_entry_max_bytes": 12345}"#,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let (status, _) = post_settings(
            state.clone(),
            &token,
            r#"{"archive_entry_max_bytes": null}"#,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let v = get_json_state(state, "/api/settings").await;
        assert!(
            v["archive_entry_max_bytes"].is_null(),
            "an explicit null must mean unlimited, not 'leave it alone': {v}"
        );
    }

    #[tokio::test]
    async fn merged_validation_rejects_a_per_archive_bound_that_exceeds_the_stored_total() {
        // Validating the request in isolation would miss this: the request only mentions
        // archive_buffer_max_bytes, so a check against the request alone has nothing to compare
        // it to. The stored archive_total_buffer_bytes must be part of what gets validated.
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let token = state.csrf_token.clone();

        let (status, _) = post_settings(
            state.clone(),
            &token,
            r#"{"archive_total_buffer_bytes": 1000000}"#,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let (status, msg) =
            post_settings(state, &token, r#"{"archive_buffer_max_bytes": 2000000}"#).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            msg.contains("archive_total_buffer_bytes"),
            "the refusal must name the stored limit it was checked against: {msg}"
        );
    }

    #[tokio::test]
    async fn pending_formats_are_listed_and_resolving_updates_the_lists() {
        let _scope = ScopedDataDir::new();
        let (_t, db, state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            cat.record_pending_format("vol-1", "a.bak", "bak", 4096, 10)
                .unwrap();
        }
        let v = get_json(&db, "/api/pending-formats").await;
        assert_eq!(v[0]["extension"], "bak");
        assert_eq!(v[0]["count"], 1);

        let token = state.csrf_token.clone();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pending-formats/resolve")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", token)
                    .body(Body::from(r#"{"extension":"bak","action":"descend"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        let v = get_json(&db, "/api/pending-formats").await;
        assert!(
            v.as_array().unwrap().is_empty(),
            "resolved formats stop being reported"
        );
    }

    #[tokio::test]
    async fn resolving_a_format_without_a_csrf_token_is_refused() {
        let _scope = ScopedDataDir::new();
        let (_t, _db, state) = seed_dupes();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pending-formats/resolve")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"extension":"bak","action":"descend"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn resolving_a_format_keeps_the_default_deny_list() {
        // The trap: with no stored list, the EFFECTIVE deny-list is the compiled-in default. If the
        // handler appends to an empty vec instead of seeding from that default, one click silently
        // drops .docx/.jar back onto the descend path -- invisible until a later scan explodes
        // every Office document into its parts.
        let _scope = ScopedDataDir::new();
        let (_t, db, state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            cat.record_pending_format("vol-1", "a.kra", "kra", 10, 10)
                .unwrap();
        }
        let token = state.csrf_token.clone();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pending-formats/resolve")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", token)
                    .body(Body::from(r#"{"extension":"kra","action":"document"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        let cfg = crate::config::Config::default_paths().unwrap();
        let stored = crate::config::load_settings(&cfg.settings_path());
        let deny = stored
            .archive_deny_extensions
            .expect("the list was written");
        assert!(
            deny.iter().any(|e| e == "kra"),
            "the resolved format was added"
        );
        assert!(
            deny.iter().any(|e| e == "docx"),
            "the compiled-in defaults must survive: got {deny:?}"
        );
        assert!(deny.iter().any(|e| e == "jar"), "got {deny:?}");
    }

    #[tokio::test]
    async fn resolving_a_format_twice_does_not_duplicate_it() {
        let _scope = ScopedDataDir::new();
        let (_t, db, state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            cat.record_pending_format("vol-1", "a.bak", "bak", 10, 10)
                .unwrap();
        }
        let token = state.csrf_token.clone();
        let app = build_router_with(state);
        for _ in 0..2 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/pending-formats/resolve")
                        .header("content-type", "application/json")
                        .header("x-cleanup-token", token.clone())
                        .body(Body::from(r#"{"extension":"bak","action":"descend"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::OK);
        }

        let cfg = crate::config::Config::default_paths().unwrap();
        let stored = crate::config::load_settings(&cfg.settings_path());
        let allow = stored.archive_allow_extensions.unwrap();
        assert_eq!(
            allow.iter().filter(|e| *e == "bak").count(),
            1,
            "resolving twice must not duplicate: got {allow:?}"
        );
    }

    #[tokio::test]
    async fn resolving_a_pending_format_with_no_extension_round_trips() {
        // An extensionless zip-format file records a pending row with extension == "". That is a
        // legitimate domain value ("no extension"), not a malformed request -- resolving it must
        // work the same as any other extension, not be rejected as "missing".
        let _scope = ScopedDataDir::new();
        let (_t, db, state) = seed_dupes();
        {
            let cat = Catalog::open(&db).unwrap();
            cat.record_pending_format("vol-1", "a", "", 10, 10).unwrap();
        }
        let v = get_json(&db, "/api/pending-formats").await;
        assert_eq!(v[0]["extension"], "");

        let token = state.csrf_token.clone();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pending-formats/resolve")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", token)
                    .body(Body::from(r#"{"extension":"","action":"document"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        let v = get_json(&db, "/api/pending-formats").await;
        assert!(
            v.as_array().unwrap().is_empty(),
            "resolved formats stop being reported"
        );
    }

    #[tokio::test]
    async fn pending_formats_endpoint_degrades_gracefully_on_a_pre_migration_catalog() {
        // Catalogs created before this branch have no pending_archive_formats table.
        // `Catalog::open_readonly` runs no schema DDL, so the GET handler must not 500 on a
        // "no such table" error -- it degrades to an empty list.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("old.db");
        {
            // A minimal catalog with no pending_archive_formats table at all.
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE dummy(x INTEGER);")
                .unwrap();
        }
        let v = get_json(&db, "/api/pending-formats").await;
        assert!(
            v.as_array().unwrap().is_empty(),
            "a pre-migration catalog should report no pending formats, not 500: {v}"
        );
    }

    #[tokio::test]
    async fn pending_formats_endpoint_surfaces_a_genuine_open_failure() {
        // A directory used as the db path is a clean way to force a real OS/sqlite open error,
        // distinct from "no catalog yet". This must NOT read as "nothing pending" -- that would be
        // indistinguishable from a clean drive to the user.
        let tmp = tempfile::tempdir().unwrap();
        let bogus_db = tmp.path().join("looks-like-a-db-but-is-a-directory");
        std::fs::create_dir_all(&bogus_db).unwrap();

        let app = build_router(bogus_db);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/pending-formats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            res.status(),
            axum::http::StatusCode::OK,
            "a genuine open failure must not be reported as an empty pending list"
        );
    }

    #[tokio::test]
    async fn settings_endpoint_exposes_archive_extension_lists() {
        let _scope = ScopedDataDir::new();
        let (_t, db, _state) = seed_dupes();
        let v = get_json(&db, "/api/settings").await;
        let deny = v["archive_deny_extensions"].as_array().unwrap();
        assert!(
            deny.iter().any(|e| e == "docx"),
            "the effective deny-list must include the compiled-in default: {deny:?}"
        );
    }

    // Two identical folders on one volume, plus a zip whose inside also duplicates a loose folder.
    fn seed_identical_trees() -> (tempfile::TempDir, PathBuf, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("driveA");
        std::fs::create_dir_all(drive.join("orig")).unwrap();
        std::fs::create_dir_all(drive.join("copy")).unwrap();
        std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
        std::fs::write(drive.join("orig/a.txt"), b"SAME").unwrap();
        std::fs::write(drive.join("copy/a.txt"), b"SAME").unwrap();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "Photos HDD".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let mk = |path: &str| crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "txt".into(),
                size_bytes: 4,
                content_hash: "SAMEHASH".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&mk("orig/a.txt"), 100).unwrap();
            cat.upsert_file(&mk("copy/a.txt"), 100).unwrap();
            cat.rebuild_directory_trees("vol-1", 100).unwrap();
        }
        let mut mounts = HashMap::new();
        mounts.insert("vol-1".to_string(), drive);
        let state = AppState {
            catalog_path: db.clone(),
            mounts: crate::mounts::MountResolver::Fixed(mounts.clone()),
            csrf_token: "T".into(),
            scan_queue: crate::scan_queue::ScanQueue::new(db.clone()),
            // Same resolver the rest of the state uses. Handing the queue an empty one would make
            // every job fail with "drive not connected" while the request path looked fine.
            quarantine_queue: crate::quarantine_queue::QuarantineQueue::new(
                db.clone(),
                crate::mounts::MountResolver::Fixed(mounts),
            ),
        };
        (tmp, db, state)
    }

    #[tokio::test]
    async fn a_mutation_snapshots_beside_its_own_catalogue_not_the_ambient_one() {
        // #44: this route used to resolve Config::default_paths() for its snapshot destination, so
        // a request that mutated a temp catalogue wrote its snapshot into whichever data directory
        // the environment pointed at. In this project that meant `cargo test` evicting the user's
        // genuine pre-migration snapshots -- the documented rollback path for a schema migration.
        // The queue's drain (src/quarantine_queue.rs) now calls `catalog::backup::snapshot_beside`,
        // which derives the backups directory from the catalogue path itself; this test exercises
        // that call.
        //
        // The ambient directory here stands in for the user's real one: it must stay empty.
        let ambient = ScopedDataDir::new();
        let (_t, db, state) = seed_dupes();

        let id: i64 = {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.conn
                .query_row(
                    "SELECT id FROM files WHERE relative_path='copy/a.jpg'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        tokio::spawn(state.quarantine_queue.clone().run_worker());
        let app = build_router_with(state);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/quarantine")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", "T")
                    .body(axum::body::Body::from(format!(
                        r#"{{"quarantine_ids":[{id}]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        // `catalog::backup::snapshot` opens its destination with the default DELETE journal mode,
        // so while the backup copy is running the directory transiently holds both
        // `catalog-{now}.db` and `catalog-{now}.db-journal` -- the journal disappears once the
        // backup connection drops. Counting only the `.db` file itself (excluding `-journal`/
        // `-wal`/`-shm` companions) is what removes that race: a reader that lands mid-backup
        // still can't see a `.db` file until the rename that publishes it completes, so a single
        // poll landing on count == 1 is already the final state, not a snapshot of an in-progress
        // write. (An earlier version of this test also required the count to "settle" across two
        // consecutive polls; with only one snapshot ever taken and the `.db` filter already
        // excluding the transient journal, that check could never distinguish anything and has
        // been dropped.)
        let own = db.parent().unwrap().join("catalog.backups");
        let count_snapshots = |dir: &std::path::Path| -> usize {
            std::fs::read_dir(dir)
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
                        .count()
                })
                .unwrap_or(0)
        };
        let mut n_own = 0;
        for _ in 0..400 {
            n_own = count_snapshots(&own);
            if n_own == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            n_own, 1,
            "the snapshot belongs beside the catalogue being mutated"
        );

        let n_ambient = std::fs::read_dir(ambient.path().join("catalog.backups"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(
            n_ambient, 0,
            "nothing may be written to the ambient data directory -- that is the user's real one"
        );
    }

    // Two identical loose folders (actionable) PLUS a pair that exists only inside an archive
    // (blocked), so a test can tell the two apart.
    fn seed_identical_archive_trees() -> (tempfile::TempDir, PathBuf, AppState) {
        let (tmp, db, state) = seed_identical_trees();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            // One archive holding the same folder twice under different names. Its own row makes it
            // an archive; the entries make it a directory.
            cat.upsert_file(
                &crate::catalog::models::NewFile {
                    volume_id: "vol-1".into(),
                    relative_path: "backup.zip".into(),
                    filename: "backup.zip".into(),
                    extension: "zip".into(),
                    size_bytes: 9,
                    content_hash: "ZIPHASH".into(),
                    created_time: None,
                    modified_time: None,
                    accessed_time: None,
                    category: crate::category::Category::Other,
                    container_chain: None,
                },
                100,
            )
            .unwrap();
            for chain in ["Alpha/x.txt", "Beta/x.txt"] {
                cat.conn
                    .execute(
                        "INSERT INTO files(volume_id, relative_path, filename, extension,
                             size_bytes, content_hash, category, container_chain, status,
                             first_seen_at, last_seen_at)
                         VALUES ('vol-1','backup.zip','x.txt','txt',64,'INNERHASH','document',
                                 ?1,'active',100,100)",
                        rusqlite::params![chain],
                    )
                    .unwrap();
            }
            cat.rebuild_directory_trees("vol-1", 100).unwrap();
        }
        (tmp, db, state)
    }

    #[tokio::test]
    async fn tree_duplicates_lists_a_group_with_its_blast_radius() {
        let (_t, _db, state) = seed_identical_trees();
        let v = get_json_state(state, "/api/tree-duplicates").await;
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "orig and copy hold the same file");
        assert_eq!(groups[0]["members"].as_array().unwrap().len(), 2);
        // The UI cannot show a blast radius without these two, and the whole mitigation for making
        // decisions coarser is showing the blast radius before confirming.
        assert!(groups[0]["file_count"].as_i64().unwrap() > 0);
        assert!(groups[0]["reclaimable_bytes"].as_i64().unwrap() > 0);
        let m = &groups[0]["members"][0];
        assert_eq!(m["volume_label"], "Photos HDD");
        assert_eq!(m["needs_repack"], false);
        assert_eq!(m["mounted"], true);
    }

    #[tokio::test]
    async fn tree_duplicates_shows_the_name_the_user_set_not_the_detected_label() {
        // #62: both real drives were first seen as `D:\`, so a cross-drive pair rendered as two
        // identical rows with identical Quarantine buttons -- no way to tell which physical drive
        // was about to be emptied, across 255 groups worth 1.86 TB. The display name exists to
        // disambiguate exactly this, and this view was reading the detected label instead.
        let (_t, db, state) = seed_identical_trees();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.set_volume_meta("vol-1", Some("Uni Big"), None, 200)
                .unwrap();
        }
        let v = get_json_state(state, "/api/tree-duplicates").await;
        let m = &v["groups"][0]["members"][0];
        assert_eq!(
            m["volume_label"], "Uni Big",
            "the folder view must show the name the user set, or two drives are indistinguishable"
        );
    }

    #[tokio::test]
    async fn tree_duplicates_flags_and_ranks_actionable_groups_first() {
        // #59: on the real 4 TB scan, 73% of rendered folder entries were inside archives and could
        // not be acted on. A group is actionable only when at least one copy is a loose folder --
        // something that can actually be quarantined while another copy survives.
        let (_t, _db, state) = seed_identical_archive_trees();
        let v = get_json_state(state, "/api/tree-duplicates").await;
        let groups = v["groups"].as_array().unwrap();
        assert!(!groups.is_empty());
        for g in groups {
            let any_loose = g["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["needs_repack"] == false);
            assert_eq!(
                g["actionable"], any_loose,
                "actionable must mean 'at least one copy can be moved'"
            );
        }
        // Actionable groups sort ahead of blocked ones, whatever their size.
        let flags: Vec<bool> = groups
            .iter()
            .map(|g| g["actionable"].as_bool().unwrap())
            .collect();
        let first_blocked = flags.iter().position(|a| !a);
        if let Some(i) = first_blocked {
            assert!(
                flags[i..].iter().all(|a| !a),
                "an actionable group must never sort after a blocked one: {flags:?}"
            );
        }
    }

    #[tokio::test]
    async fn quarantine_tree_requires_csrf_token() {
        // Same contract as every other mutating endpoint.
        let (_t, _db, state) = seed_identical_trees();
        let app = build_router_with(state);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/quarantine-tree")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"volume_id":"vol-1","path":"copy"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(
            _t.path().join("driveA/copy/a.txt").exists(),
            "a rejected request must not have moved anything"
        );
    }

    #[tokio::test]
    async fn quarantining_is_queued_and_the_worker_completes_it() {
        // The POST now ENQUEUES rather than doing the work, so a reviewer can confirm the next
        // folder immediately instead of waiting (#66). The request returns a position; the worker
        // does the move. Both halves matter, so this drives the whole path.
        let (_t, db, state) = seed_identical_trees();
        tokio::spawn(state.quarantine_queue.clone().run_worker());
        let app = build_router_with(state.clone());
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/quarantine-tree")
                    .header("content-type", "application/json")
                    .header("x-cleanup-token", "T")
                    .body(axum::body::Body::from(
                        r#"{"volume_id":"vol-1","path":"copy"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 100_000)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["queued"], true,
            "the request must return immediately, queued"
        );
        assert_eq!(v["position"], 0, "nothing else pending, so it starts next");

        // The worker runs on its own; wait for the move rather than assuming an instant.
        let moved = _t.path().join("driveA/_ToDelete/copy/a.txt");
        for _ in 0..200 {
            if moved.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            moved.is_file(),
            "the queued quarantine must actually happen"
        );
        assert!(!_t.path().join("driveA/copy").exists());

        let cat = crate::catalog::Catalog::open(&db).unwrap();
        let rows = cat.quarantined_rows("vol-1").unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the catalogue must be updated, not just the disk"
        );
    }

    #[tokio::test]
    async fn several_folders_can_be_queued_without_waiting_for_each_other() {
        // The actual complaint: with 1,201 decisions to make, confirming one must not block the
        // next. Three requests, none of which waits for the work to finish.
        let (_t, _db, state) = seed_identical_trees();
        let q = state.quarantine_queue.clone();
        for p in ["copy", "orig", "copy"] {
            let app = build_router_with(state.clone());
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/quarantine-tree")
                        .header("content-type", "application/json")
                        .header("x-cleanup-token", "T")
                        .body(axum::body::Body::from(format!(
                            r#"{{"volume_id":"vol-1","path":"{p}"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::OK);
        }
        // Worker deliberately not started: this asserts the REQUESTS do not block on the work.
        let st = q.status();
        assert_eq!(
            st.pending.len(),
            2,
            "two distinct folders queued; the repeated one must not queue twice"
        );
    }

    #[tokio::test]
    async fn per_file_duplicates_exclude_entries_inside_archives() {
        // Not actionable one at a time: you cannot delete a file inside a zip without repacking it.
        // Already true of the dedup queries; this locks it so descending into archives for TREE
        // matching never leaks 43,000 unactionable rows into the per-file queue.
        let (_t, db) = seed_catalog();
        let v = get_json(&db, "/api/duplicates").await;
        for g in v["groups"].as_array().unwrap() {
            for f in g["files"].as_array().unwrap_or(&vec![]) {
                assert!(
                    f["container_chain"].is_null(),
                    "archive entries must not appear in the per-file duplicate queue: {f}"
                );
            }
        }
    }
}
