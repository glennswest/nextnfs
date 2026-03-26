use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    // Legacy single-export (backwards-compatible)
    #[serde(default)]
    pub export: Option<ExportConfig>,
    // Multi-export array
    #[serde(default)]
    pub exports: Vec<ExportEntry>,
}

#[derive(Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_api_listen")]
    pub api_listen: String,
    /// Directory for state recovery files (near-zero grace period)
    #[serde(default)]
    pub state_dir: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct ExportConfig {
    #[serde(default = "default_export_path")]
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ExportEntry {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
    /// Maximum operations per second (0 = unlimited)
    #[serde(default)]
    pub max_ops_per_sec: u64,
    /// Maximum bytes per second for reads+writes (0 = unlimited)
    #[serde(default)]
    pub max_bytes_per_sec: u64,
}

fn default_listen() -> String {
    "0.0.0.0:2049".to_string()
}

fn default_api_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_export_path() -> String {
    "/export".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            api_listen: default_api_listen(),
            state_dir: None,
        }
    }
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            path: default_export_path(),
            read_only: false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Merge singular [export] into [[exports]] array.
    /// If [export] is present and no [[exports]] entry has the same path, add it.
    pub fn resolved_exports(&self) -> Vec<ExportEntry> {
        let mut exports = self.exports.clone();
        if let Some(ref single) = self.export {
            let already_present = exports.iter().any(|e| e.path == single.path);
            if !already_present {
                let name = Path::new(&single.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "export".to_string());
                exports.insert(
                    0,
                    ExportEntry {
                        name,
                        path: single.path.clone(),
                        read_only: single.read_only,
                        max_ops_per_sec: 0,
                        max_bytes_per_sec: 0,
                    },
                );
            }
        }
        exports
    }
}
