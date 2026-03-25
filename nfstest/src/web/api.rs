use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::harness::{self, ProgressEvent, RunConfig, RunResult, RunSummary, TestDef};

#[derive(Deserialize)]
pub struct RunRequest {
    server: String,
    port: Option<u16>,
    export: Option<String>,
    version: Option<String>,
    layer: Option<String>,
    tag: Option<String>,
    test_id: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
}

#[derive(Serialize)]
pub struct RunStarted {
    run_id: String,
    total_tests: usize,
}

#[derive(Serialize)]
pub struct StatusResponse {
    status: String,
    run_id: Option<String>,
    total_tests: Option<usize>,
    completed: Option<usize>,
}

#[derive(Serialize)]
pub struct RunListEntry {
    run_id: String,
    started_at: String,
    finished_at: String,
    server: String,
    port: u16,
    export: String,
    summary: RunSummary,
}

pub async fn list_tests() -> Json<Vec<TestDef>> {
    Json(crate::wire::registry())
}

pub async fn start_run(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunStarted>, StatusCode> {
    // Check if a run is already active
    {
        let active = state.active_run.read().await;
        if active.is_some() {
            return Err(StatusCode::CONFLICT);
        }
    }

    let version_filter = harness::parse_version_filter(req.version.as_deref().unwrap_or("all"));
    let layer_filter = harness::parse_layer_filter(req.layer.as_deref().unwrap_or("wire"));

    let config = RunConfig {
        server: req.server,
        port: req.port.unwrap_or(2049),
        export: req.export.unwrap_or_else(|| "/".to_string()),
        version_filter,
        layer_filter,
        tag_filter: req.tag,
        test_id_filter: req.test_id,
        output_dir: state.data_dir.clone(),
        uid: req.uid.unwrap_or(0),
        gid: req.gid.unwrap_or(0),
    };

    // Count matching tests
    let all_tests = crate::wire::registry();
    let total_tests = all_tests
        .iter()
        .filter(|t| {
            config.version_filter.contains(&t.version)
                && config.layer_filter.contains(&t.layer)
                && config
                    .tag_filter
                    .as_ref()
                    .map(|tag| t.tags.iter().any(|tt| *tt == tag.as_str()))
                    .unwrap_or(true)
                && config
                    .test_id_filter
                    .as_ref()
                    .map(|id| t.id == id.as_str())
                    .unwrap_or(true)
        })
        .count();

    let manager = harness::TestManager::new(config);
    let tx = state.progress_tx.clone();
    let active_run = state.active_run.clone();
    let data_dir = state.data_dir.clone();

    // Generate run ID upfront so we can return it
    let run_id = uuid::Uuid::new_v4().to_string();
    let run_id_clone = run_id.clone();

    // Set active run
    {
        let mut active = active_run.write().await;
        *active = Some(super::ActiveRun {
            run_id: run_id.clone(),
            total_tests,
            completed: 0,
            started_at: Utc::now(),
        });
    }

    // Send RunStarted event
    let _ = tx.send(ProgressEvent::RunStarted {
        run_id: run_id.clone(),
        total_tests,
    });

    // Spawn test execution task
    tokio::spawn(async move {
        let result = manager.run_with_progress(tx.clone(), active_run.clone()).await;

        match result {
            Ok(run_result) => {
                // Write JSON report
                let filename = format!("report-{}.json", run_result.run_id);
                let path = data_dir.join(&filename);
                if let Ok(json) = serde_json::to_string_pretty(&run_result) {
                    let _ = std::fs::write(&path, json);
                }

                let _ = tx.send(ProgressEvent::RunFinished {
                    run_id: run_result.run_id,
                    summary: run_result.summary,
                });
            }
            Err(e) => {
                tracing::error!("Test run failed: {}", e);
                let _ = tx.send(ProgressEvent::RunFinished {
                    run_id: run_id_clone,
                    summary: RunSummary {
                        total: 0,
                        passed: 0,
                        failed: 0,
                        skipped: 0,
                        errors: 1,
                        duration: std::time::Duration::ZERO,
                    },
                });
            }
        }

        // Clear active run
        let mut active = active_run.write().await;
        *active = None;
    });

    Ok(Json(RunStarted {
        run_id,
        total_tests,
    }))
}

pub async fn list_runs(State(state): State<Arc<AppState>>) -> Json<Vec<RunListEntry>> {
    let mut runs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&state.data_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_str().unwrap_or("");
            if name.starts_with("report-") && name.ends_with(".json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(result) = serde_json::from_str::<RunResult>(&content) {
                        runs.push(RunListEntry {
                            run_id: result.run_id,
                            started_at: result.started_at.to_rfc3339(),
                            finished_at: result.finished_at.to_rfc3339(),
                            server: result.server,
                            port: result.port,
                            export: result.export,
                            summary: result.summary,
                        });
                    }
                }
            }
        }
    }

    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Json(runs)
}

pub async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Result<Json<RunResult>, StatusCode> {
    let path = state.data_dir.join(format!("report-{run_id}.json"));
    let content = std::fs::read_to_string(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let result: RunResult =
        serde_json::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(result))
}

pub async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let path = state.data_dir.join(format!("report-{run_id}.json"));
    std::fs::remove_file(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let active = state.active_run.read().await;
    match &*active {
        Some(run) => Json(StatusResponse {
            status: "running".to_string(),
            run_id: Some(run.run_id.clone()),
            total_tests: Some(run.total_tests),
            completed: Some(run.completed),
        }),
        None => Json(StatusResponse {
            status: "idle".to_string(),
            run_id: None,
            total_tests: None,
            completed: None,
        }),
    }
}
