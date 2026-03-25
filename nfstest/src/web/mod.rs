mod api;
mod css;
mod pages;
mod sse;

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, RwLock};

use crate::harness::ProgressEvent;

pub struct ActiveRun {
    pub run_id: String,
    pub total_tests: usize,
    pub completed: usize,
    pub started_at: DateTime<Utc>,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub active_run: Arc<RwLock<Option<ActiveRun>>>,
    pub progress_tx: broadcast::Sender<ProgressEvent>,
    pub base_path: String,
}

pub async fn serve(bind: &str, data_dir: PathBuf, base_path: String) -> anyhow::Result<()> {
    // Ensure data directory exists
    std::fs::create_dir_all(&data_dir)?;

    let (tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        data_dir,
        active_run: Arc::new(RwLock::new(None)),
        progress_tx: tx,
        base_path,
    });

    let app = Router::new()
        // HTML pages
        .route("/", get(pages::dashboard))
        .route("/run", get(pages::run_form))
        .route("/results", get(pages::results_list))
        .route("/results/{run_id}", get(pages::result_detail))
        // REST API
        .route("/api/tests", get(api::list_tests))
        .route("/api/run", post(api::start_run))
        .route("/api/runs", get(api::list_runs))
        .route("/api/runs/{run_id}", get(api::get_run).delete(api::delete_run))
        .route("/api/status", get(api::status))
        // SSE
        .route("/api/progress", get(sse::progress_stream))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("nextnfstest web UI listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}
