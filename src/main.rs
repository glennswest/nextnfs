use std::path::PathBuf;

use clap::{Parser, Subcommand};
use nextnfs_server::export_manager::ExportManagerHandle;
use nextnfs_server::NFSServer;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod api;
mod cli;
mod config;
mod web;

#[derive(Parser)]
#[command(name = "nextnfs", about = "High-performance NFSv4 server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to export as NFS root (shorthand for `serve --export`)
    #[arg(short, long, default_value = "/export", global = true)]
    export: PathBuf,

    /// NFS listen address
    #[arg(short, long, default_value = "0.0.0.0:2049", global = true)]
    listen: String,

    /// REST API listen address
    #[arg(short, long, default_value = "0.0.0.0:8080", global = true)]
    api_listen: String,

    /// Config file path
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the NFS server (default)
    Serve {
        /// Path to export as NFS root
        #[arg(short, long)]
        export: Option<PathBuf>,

        /// NFS listen address
        #[arg(short, long)]
        listen: Option<String>,

        /// REST API listen address
        #[arg(short, long)]
        api_listen: Option<String>,

        /// Config file path
        #[arg(short, long)]
        config: Option<PathBuf>,
    },

    /// Manage exports
    Export {
        #[command(subcommand)]
        action: ExportAction,
    },

    /// Show server statistics
    Stats {
        /// API URL
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        api: String,
    },

    /// Check server health
    Health {
        /// API URL
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        api: String,
    },
}

#[derive(Subcommand)]
enum ExportAction {
    /// List all exports
    List {
        /// API URL
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        api: String,
    },
    /// Add an export
    Add {
        /// Export name
        #[arg(long)]
        name: String,
        /// Filesystem path to export
        #[arg(long)]
        path: String,
        /// Make export read-only
        #[arg(long)]
        read_only: bool,
        /// API URL
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        api: String,
    },
    /// Remove an export
    Remove {
        /// Export name
        #[arg(long)]
        name: String,
        /// API URL
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        api: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .compact()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Export { action }) => match action {
            ExportAction::List { api } => cli::export_list(&api).await,
            ExportAction::Add {
                name,
                path,
                read_only,
                api,
            } => cli::export_add(&api, &name, &path, read_only).await,
            ExportAction::Remove { name, api } => cli::export_remove(&api, &name).await,
        },
        Some(Commands::Stats { api }) => cli::stats(&api).await,
        Some(Commands::Health { api }) => cli::health(&api).await,
        Some(Commands::Serve {
            export,
            listen,
            api_listen,
            config,
        }) => {
            run_server(
                export.unwrap_or(cli.export),
                listen.unwrap_or(cli.listen),
                api_listen.unwrap_or(cli.api_listen),
                config.or(cli.config),
            )
            .await;
        }
        None => {
            // Default: run server (backwards-compatible)
            run_server(cli.export, cli.listen, cli.api_listen, cli.config).await;
        }
    }
}

async fn run_server(
    export_path: PathBuf,
    listen_addr: String,
    api_listen_addr: String,
    config_path: Option<PathBuf>,
) {
    // Load config and merge with CLI args
    let (exports, listen, api_listen) = if let Some(config_path) = config_path {
        match config::Config::load(&config_path) {
            Ok(cfg) => {
                let resolved = cfg.resolved_exports();
                let listen = if listen_addr != "0.0.0.0:2049" {
                    listen_addr
                } else {
                    cfg.server.listen
                };
                let api = if api_listen_addr != "0.0.0.0:8080" {
                    api_listen_addr
                } else {
                    cfg.server.api_listen
                };
                if resolved.is_empty() {
                    // No exports in config — use CLI export path
                    let name = export_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "export".to_string());
                    (
                        vec![config::ExportEntry {
                            name,
                            path: export_path.display().to_string(),
                            read_only: false,
                        }],
                        listen,
                        api,
                    )
                } else {
                    (resolved, listen, api)
                }
            }
            Err(e) => {
                error!("Failed to load config: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        let name = export_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "export".to_string());
        (
            vec![config::ExportEntry {
                name,
                path: export_path.display().to_string(),
                read_only: false,
            }],
            listen_addr,
            api_listen_addr,
        )
    };

    // Create ExportManager and register exports
    let export_manager = ExportManagerHandle::new();
    for entry in &exports {
        let path = PathBuf::from(&entry.path);
        // Validate path exists
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                error!(
                    "Export path {} does not exist: {}",
                    entry.path, e
                );
                std::process::exit(1);
            }
        };
        if !canonical.is_dir() {
            error!("Export path {} is not a directory", canonical.display());
            std::process::exit(1);
        }

        match export_manager
            .add_export(entry.name.clone(), canonical.clone(), entry.read_only)
            .await
        {
            Ok(info) => {
                info!(
                    name = %info.name,
                    path = %info.path.display(),
                    export_id = info.export_id,
                    read_only = info.read_only,
                    "export registered"
                );
            }
            Err(e) => {
                error!("Failed to add export '{}': {}", entry.name, e);
                std::process::exit(1);
            }
        }
    }

    // Build NFS server
    let mut builder = NFSServer::builder();
    builder.bind(&listen);
    builder.export_manager(export_manager.clone());
    let server = builder.build();

    info!(
        nfs_listen = %listen,
        api_listen = %api_listen,
        exports = exports.len(),
        "starting nextnfs"
    );

    // Start API server and NFS server concurrently
    let api_em = export_manager.clone();
    let api_bind = api_listen.clone();

    let server_task = tokio::spawn(async move {
        server.start_async().await;
    });

    let api_task = tokio::spawn(async move {
        api::start_api_server(api_bind, api_em).await;
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT, shutting down");
        }
        result = server_task => {
            if let Err(e) = result {
                error!("NFS server task failed: {:?}", e);
            }
        }
        result = api_task => {
            if let Err(e) = result {
                error!("API server task failed: {:?}", e);
            }
        }
    }
}
