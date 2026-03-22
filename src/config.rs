use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub export: ExportConfig,
}

#[derive(Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
}

#[derive(Deserialize)]
pub struct ExportConfig {
    #[serde(default = "default_export_path")]
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

fn default_listen() -> String {
    "0.0.0.0:2049".to_string()
}

fn default_export_path() -> String {
    "/export".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
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
}
