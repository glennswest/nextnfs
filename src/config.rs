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
    /// TLS certificate file path (PEM) for RPC-over-TLS (RFC 9289)
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// TLS private key file path (PEM) for RPC-over-TLS (RFC 9289)
    #[serde(default)]
    pub tls_key: Option<String>,
    /// RDMA device name for NFS-over-RDMA (RFC 8166/8267), e.g. "mlx5_0"
    #[serde(default)]
    pub rdma_device: Option<String>,
    /// RDMA listen port (default 20049, per RFC 8267 §5.2.1)
    #[serde(default)]
    pub rdma_port: Option<u16>,
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
    /// Allowed client IP addresses or CIDR subnets (empty = allow all)
    #[serde(default)]
    pub clients: Vec<String>,
    /// UID/GID squash mode: "none", "root_squash", "all_squash"
    #[serde(default)]
    pub squash: String,
    /// Anonymous UID for squashed requests (default 65534)
    #[serde(default = "default_anon_uid")]
    pub anon_uid: u32,
    /// Anonymous GID for squashed requests (default 65534)
    #[serde(default = "default_anon_gid")]
    pub anon_gid: u32,
}

fn default_anon_uid() -> u32 { 65534 }
fn default_anon_gid() -> u32 { 65534 }

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
            tls_cert: None,
            tls_key: None,
            rdma_device: None,
            rdma_port: None,
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
                        clients: vec![],
                        squash: String::new(),
                        anon_uid: 65534,
                        anon_gid: 65534,
                    },
                );
            }
        }
        exports
    }
}
