use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// NFS protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NfsVersion {
    V3,
    #[serde(rename = "4.0")]
    V4_0,
    #[serde(rename = "4.1")]
    V4_1,
    #[serde(rename = "4.2")]
    V4_2,
}

impl std::fmt::Display for NfsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NfsVersion::V3 => write!(f, "NFSv3"),
            NfsVersion::V4_0 => write!(f, "NFSv4.0"),
            NfsVersion::V4_1 => write!(f, "NFSv4.1"),
            NfsVersion::V4_2 => write!(f, "NFSv4.2"),
        }
    }
}

/// Test layer classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TestLayer {
    Wire,
    Functional,
    Interop,
    Stress,
    Perf,
}

impl std::fmt::Display for TestLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestLayer::Wire => write!(f, "wire"),
            TestLayer::Functional => write!(f, "functional"),
            TestLayer::Interop => write!(f, "interop"),
            TestLayer::Stress => write!(f, "stress"),
            TestLayer::Perf => write!(f, "perf"),
        }
    }
}

/// Result status for a single test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestStatus {
    Pass,
    Fail,
    Skip,
    Error,
}

impl std::fmt::Display for TestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestStatus::Pass => write!(f, "PASS"),
            TestStatus::Fail => write!(f, "FAIL"),
            TestStatus::Skip => write!(f, "SKIP"),
            TestStatus::Error => write!(f, "ERROR"),
        }
    }
}

/// Configuration for a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    pub server: String,
    pub port: u16,
    pub export: String,
    pub version_filter: Vec<NfsVersion>,
    pub layer_filter: Vec<TestLayer>,
    pub tag_filter: Option<String>,
    pub test_id_filter: Option<String>,
    pub output_dir: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// Static definition of a test case.
#[derive(Debug, Clone, Serialize)]
pub struct TestDef {
    pub id: &'static str,
    pub description: &'static str,
    pub version: NfsVersion,
    pub layer: TestLayer,
    pub tags: Vec<&'static str>,
}

/// Result of executing a single test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub id: String,
    pub description: String,
    pub version: NfsVersion,
    pub layer: TestLayer,
    pub status: TestStatus,
    pub duration: Duration,
    pub message: Option<String>,
    pub detail: Option<String>,
}

/// Summary statistics for a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub duration: Duration,
}

/// Complete result of a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub server: String,
    pub port: u16,
    pub export: String,
    pub summary: RunSummary,
    pub results: Vec<TestResult>,
    pub output_dir: PathBuf,
}

/// Progress events sent via SSE during a live test run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ProgressEvent {
    RunStarted {
        run_id: String,
        total_tests: usize,
    },
    TestCompleted {
        index: usize,
        total: usize,
        result: TestResult,
    },
    RunFinished {
        run_id: String,
        summary: RunSummary,
    },
}
