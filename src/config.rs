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
    /// Kerberos keytab (MIT format) holding the server's `nfs/<host>@REALM`
    /// keys. Set: RPCSEC_GSS (krb5, krb5i, krb5p) is served; unset: it is
    /// refused (#106).
    #[serde(default)]
    pub krb5_keytab: Option<String>,
    /// Kerberos principal → uid/gid, checked before `/etc/passwd`.
    #[serde(default)]
    pub krb5_map: Vec<Krb5Map>,
    /// File holding the bearer token the REST API's write routes (add or
    /// remove an export, set QoS) require. Unset: the API refuses every
    /// change (#122).
    #[serde(default)]
    pub api_token_file: Option<String>,
    /// Directories an export added through the API must be inside. Empty:
    /// any directory (the token holder is trusted with the host).
    #[serde(default)]
    pub api_export_roots: Vec<String>,
}

/// `[[server.krb5_map]]`: the identity a Kerberos principal's calls run as.
#[derive(Deserialize, Clone, Debug)]
pub struct Krb5Map {
    /// Full principal, `name[/instance]@REALM`.
    pub principal: String,
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups.
    #[serde(default)]
    pub gids: Vec<u32>,
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
    /// UID/GID squash mode: "root_squash" (default when empty), "all_squash",
    /// "none". Permissions are checked for the mapped identity (#91).
    #[serde(default)]
    pub squash: String,
    /// Anonymous UID for squashed requests (default 65534)
    #[serde(default = "default_anon_uid")]
    pub anon_uid: u32,
    /// Anonymous GID for squashed requests (default 65534)
    #[serde(default = "default_anon_gid")]
    pub anon_gid: u32,
    /// Security flavours clients may use: "sys", "krb5", "krb5i", "krb5p"
    /// (exports(5) `sec=`), in order of preference. Empty: ["sys"] (#106).
    #[serde(default)]
    pub sec: Vec<String>,
}

fn default_anon_uid() -> u32 { 65534 }
fn default_anon_gid() -> u32 { 65534 }

fn default_listen() -> String {
    "0.0.0.0:2049".to_string()
}

/// Where the REST API and web UI listen unless told otherwise: loopback
/// only (#125). A deployment that wants them on the network says so
/// (`api_listen = "0.0.0.0:8080"`, `--api-listen`).
pub const DEFAULT_API_LISTEN: &str = "127.0.0.1:8080";

fn default_api_listen() -> String {
    DEFAULT_API_LISTEN.to_string()
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
            krb5_keytab: None,
            krb5_map: Vec::new(),
            api_token_file: None,
            api_export_roots: Vec::new(),
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
                        sec: vec![],
                    },
                );
            }
        }
        exports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn krb5_keys_parse() {
        let cfg: Config = toml::from_str(
            r#"
[server]
krb5_keytab = "/etc/nextnfs/krb5.keytab"

[[server.krb5_map]]
principal = "alice@EXAMPLE.LO"
uid = 1001
gid = 1001
gids = [10]

[[exports]]
name = "secure"
path = "/srv/secure"
sec = ["krb5p", "krb5i"]

[[exports]]
name = "plain"
path = "/srv/plain"
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.krb5_keytab.as_deref(), Some("/etc/nextnfs/krb5.keytab"));
        assert_eq!(cfg.server.krb5_map.len(), 1);
        assert_eq!(cfg.server.krb5_map[0].gids, vec![10]);
        assert_eq!(cfg.exports[0].sec, vec!["krb5p", "krb5i"]);
        assert!(cfg.exports[1].sec.is_empty());
        let cfg: Config = toml::from_str("[server]\n").unwrap();
        assert!(cfg.server.krb5_keytab.is_none() && cfg.server.krb5_map.is_empty());
    }

    #[test]
    fn api_admin_keys_parse() {
        let cfg: Config = toml::from_str(
            "[server]\napi_token_file = \"/etc/nextnfs/api-token\"\napi_export_roots = [\"/srv\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.server.api_token_file.as_deref(), Some("/etc/nextnfs/api-token"));
        assert_eq!(cfg.server.api_export_roots, vec!["/srv"]);
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.server.api_token_file.is_none() && cfg.server.api_export_roots.is_empty());
        // #125: the API listens on loopback unless the config says otherwise.
        assert_eq!(cfg.server.api_listen, "127.0.0.1:8080");
        assert_eq!(ServerConfig::default().api_listen, DEFAULT_API_LISTEN);
        let cfg: Config = toml::from_str("[server]\napi_listen = \"0.0.0.0:8080\"\n").unwrap();
        assert_eq!(cfg.server.api_listen, "0.0.0.0:8080");
    }
}
