use std::path::PathBuf;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{delete, get, post, put},
    Router,
};
use nextnfs_server::export_manager::{ExportManagerHandle, ExportStatsSnapshot, QosConfig};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::web;

#[derive(Clone)]
pub struct ApiState {
    pub export_manager: ExportManagerHandle,
}

#[derive(Serialize)]
struct ExportResponse {
    name: String,
    path: String,
    read_only: bool,
    export_id: u8,
    stats: ExportStatsSnapshot,
}

#[derive(Deserialize)]
pub struct AddExportRequest {
    name: String,
    path: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    exports: usize,
}

#[derive(Serialize)]
struct StatsResponse {
    total_reads: u64,
    total_writes: u64,
    total_bytes_read: u64,
    total_bytes_written: u64,
    total_ops: u64,
    exports: Vec<ExportResponse>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        // Web UI routes
        .route("/", get(ui_dashboard))
        .route("/ui/exports", get(ui_exports))
        .route("/ui/stats", get(ui_stats))
        // REST API routes
        .route("/health", get(health))
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/exports", post(add_export))
        .route("/api/v1/exports/{name}", delete(remove_export))
        .route("/api/v1/stats", get(server_stats))
        .route("/api/v1/stats/{name}", get(export_stats))
        .route("/api/v1/qos/{name}", get(get_qos))
        .route("/api/v1/qos/{name}", put(set_qos))
        .with_state(state)
}

pub async fn start_api_server(bind: String, export_manager: ExportManagerHandle) {
    let state = ApiState { export_manager };
    let app = router(state);

    let listener = TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        error!("failed to bind API server to {}: {}", bind, e);
        std::process::exit(1);
    });
    info!(%bind, "nextnfs REST API + Web UI listening");
    axum::serve(listener, app).await.unwrap();
}

// --- REST API handlers ---

async fn health(State(state): State<ApiState>) -> Json<HealthResponse> {
    let exports = state.export_manager.list_exports().await;
    Json(HealthResponse {
        status: "ok".to_string(),
        exports: exports.len(),
    })
}

async fn list_exports(State(state): State<ApiState>) -> Json<Vec<ExportResponse>> {
    let exports = state.export_manager.list_exports().await;
    let resp: Vec<ExportResponse> = exports
        .iter()
        .map(|e| ExportResponse {
            name: e.name.clone(),
            path: e.path.display().to_string(),
            read_only: e.read_only,
            export_id: e.export_id,
            stats: e.stats.snapshot(),
        })
        .collect();
    Json(resp)
}

async fn add_export(
    State(state): State<ApiState>,
    Json(req): Json<AddExportRequest>,
) -> impl IntoResponse {
    match state
        .export_manager
        .add_export(req.name.clone(), PathBuf::from(&req.path), req.read_only)
        .await
    {
        Ok(info) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "name": info.name,
                "path": info.path.display().to_string(),
                "read_only": info.read_only,
                "export_id": info.export_id,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn remove_export(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.export_manager.remove_export(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn server_stats(State(state): State<ApiState>) -> Json<StatsResponse> {
    let exports = state.export_manager.list_exports().await;
    let mut total_reads = 0u64;
    let mut total_writes = 0u64;
    let mut total_bytes_read = 0u64;
    let mut total_bytes_written = 0u64;
    let mut total_ops = 0u64;

    let export_list: Vec<ExportResponse> = exports
        .iter()
        .map(|e| {
            let s = e.stats.snapshot();
            total_reads += s.reads;
            total_writes += s.writes;
            total_bytes_read += s.bytes_read;
            total_bytes_written += s.bytes_written;
            total_ops += s.ops;
            ExportResponse {
                name: e.name.clone(),
                path: e.path.display().to_string(),
                read_only: e.read_only,
                export_id: e.export_id,
                stats: s,
            }
        })
        .collect();

    Json(StatsResponse {
        total_reads,
        total_writes,
        total_bytes_read,
        total_bytes_written,
        total_ops,
        exports: export_list,
    })
}

async fn export_stats(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let exports = state.export_manager.list_exports().await;
    match exports.iter().find(|e| e.name == name) {
        Some(e) => {
            let s = e.stats.snapshot();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "name": e.name,
                    "path": e.path.display().to_string(),
                    "read_only": e.read_only,
                    "export_id": e.export_id,
                    "stats": s,
                })),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("export '{}' not found", name) })),
        )
            .into_response(),
    }
}

// --- QoS handlers ---

async fn get_qos(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.export_manager.get_qos(&name).await {
        Some(config) => (StatusCode::OK, Json(serde_json::json!(config))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("export '{}' not found", name) })),
        )
            .into_response(),
    }
}

async fn set_qos(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(config): Json<QosConfig>,
) -> impl IntoResponse {
    match state.export_manager.set_qos(&name, config).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

// --- Web UI handlers ---

async fn ui_dashboard(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_dashboard(&exports))
}

async fn ui_exports(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_exports(&exports))
}

async fn ui_stats(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_stats(&exports))
}
