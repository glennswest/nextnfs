use std::sync::Arc;

use super::types::*;
use chrono::Utc;
use colored::Colorize;
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn};

/// The test manager orchestrates test discovery, filtering, execution, and reporting.
pub struct TestManager {
    config: RunConfig,
}

impl TestManager {
    pub fn new(config: RunConfig) -> Self {
        Self { config }
    }

    /// Run all matching tests and return the complete run result.
    pub async fn run(&self) -> anyhow::Result<RunResult> {
        self.run_inner(None, None).await
    }

    /// Run all matching tests with progress events sent via broadcast channel.
    pub async fn run_with_progress(
        &self,
        tx: broadcast::Sender<ProgressEvent>,
        active_run: Arc<RwLock<Option<crate::web::ActiveRun>>>,
    ) -> anyhow::Result<RunResult> {
        self.run_inner(Some(tx), Some(active_run)).await
    }

    async fn run_inner(
        &self,
        tx: Option<broadcast::Sender<ProgressEvent>>,
        active_run: Option<Arc<RwLock<Option<crate::web::ActiveRun>>>>,
    ) -> anyhow::Result<RunResult> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = Utc::now();
        let run_start = Instant::now();

        // Create output directory
        std::fs::create_dir_all(&self.config.output_dir)?;

        info!(
            "Starting test run {} against {}:{}{}",
            run_id, self.config.server, self.config.port, self.config.export
        );

        // Collect all test definitions
        let all_tests = crate::wire::registry();

        // Filter tests
        let tests: Vec<&TestDef> = all_tests.iter().filter(|t| self.matches(t)).collect();

        let total = tests.len();
        info!("{} tests matched filters", total);

        if total == 0 {
            warn!("No tests matched the given filters");
        }

        // Establish connection to the server
        let conn = match crate::wire::rpc::RpcClient::connect(
            &self.config.server,
            self.config.port,
            self.config.uid,
            self.config.gid,
        )
        .await
        {
            Ok(c) => Some(c),
            Err(e) => {
                error!("Failed to connect to NFS server: {}", e);
                None
            }
        };

        // Execute tests
        let mut results = Vec::with_capacity(total);
        for (i, test_def) in tests.iter().enumerate() {
            let progress = format!("[{}/{}]", i + 1, total);
            print!(
                "  {} {} {} ... ",
                progress.dimmed(),
                test_def.id.bold(),
                test_def.description
            );

            let result = self.execute_test(test_def, &conn).await;

            let status_str = match result.status {
                TestStatus::Pass => "PASS".green().bold().to_string(),
                TestStatus::Fail => "FAIL".red().bold().to_string(),
                TestStatus::Skip => "SKIP".yellow().to_string(),
                TestStatus::Error => "ERROR".red().to_string(),
            };

            println!("{} ({:?})", status_str, result.duration);

            if let Some(ref msg) = result.message {
                if result.status != TestStatus::Pass && result.status != TestStatus::Skip {
                    println!("    {}", msg.dimmed());
                }
            }

            // Send progress event if channel is available
            if let Some(ref tx) = tx {
                let _ = tx.send(ProgressEvent::TestCompleted {
                    index: i,
                    total,
                    result: result.clone(),
                });
            }

            // Update active run counter
            if let Some(ref active_run) = active_run {
                let mut active = active_run.write().await;
                if let Some(ref mut run) = *active {
                    run.completed = i + 1;
                }
            }

            results.push(result);
        }

        let total_duration = run_start.elapsed();
        let finished_at = Utc::now();

        // Compute summary
        let passed = results
            .iter()
            .filter(|r| r.status == TestStatus::Pass)
            .count();
        let failed = results
            .iter()
            .filter(|r| r.status == TestStatus::Fail)
            .count();
        let skipped = results
            .iter()
            .filter(|r| r.status == TestStatus::Skip)
            .count();
        let errors = results
            .iter()
            .filter(|r| r.status == TestStatus::Error)
            .count();

        let summary = RunSummary {
            total: results.len(),
            passed,
            failed,
            skipped,
            errors,
            duration: total_duration,
        };

        Ok(RunResult {
            run_id,
            started_at,
            finished_at,
            server: self.config.server.clone(),
            port: self.config.port,
            export: self.config.export.clone(),
            summary,
            results,
            output_dir: self.config.output_dir.clone(),
        })
    }

    /// Check if a test definition matches the current filters.
    fn matches(&self, test: &TestDef) -> bool {
        if !self.config.version_filter.contains(&test.version) {
            return false;
        }
        if !self.config.layer_filter.contains(&test.layer) {
            return false;
        }
        if let Some(ref tag) = self.config.tag_filter {
            if !test.tags.iter().any(|t| t == tag) {
                return false;
            }
        }
        if let Some(ref id) = self.config.test_id_filter {
            if test.id != id {
                return false;
            }
        }
        true
    }

    /// Execute a single test against the server.
    async fn execute_test(
        &self,
        test_def: &TestDef,
        conn: &Option<crate::wire::rpc::RpcClient>,
    ) -> TestResult {
        let start = Instant::now();

        let (status, message, detail) = match conn {
            None => (
                TestStatus::Error,
                Some("No connection to server".to_string()),
                None,
            ),
            Some(client) => match crate::wire::execute(test_def.id, client).await {
                Ok(()) => (TestStatus::Pass, None, None),
                Err(e) => {
                    let msg = format!("{}", e);
                    if msg.starts_with("SKIP:") {
                        (TestStatus::Skip, Some(msg[5..].trim().to_string()), None)
                    } else {
                        (TestStatus::Fail, Some(msg), None)
                    }
                }
            },
        };

        let duration = start.elapsed();

        TestResult {
            id: test_def.id.to_string(),
            description: test_def.description.to_string(),
            version: test_def.version,
            layer: test_def.layer,
            status,
            duration,
            message,
            detail,
        }
    }
}
