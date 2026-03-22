use std::path::PathBuf;

use clap::Parser;
use nextnfs_server::NFSServer;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use vfs::{AltrootFS, PhysicalFS, VfsPath};

mod config;

#[derive(Parser)]
#[command(name = "nextnfs", about = "High-performance NFSv4 server")]
struct Cli {
    /// Path to export as NFS root
    #[arg(short, long, default_value = "/export")]
    export: PathBuf,

    /// Listen address
    #[arg(short, long, default_value = "0.0.0.0:2049")]
    listen: String,

    /// Config file path (optional)
    #[arg(short, long)]
    config: Option<PathBuf>,
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

    // Load config file if provided, override with CLI args
    let (export_path, listen_addr) = if let Some(config_path) = &cli.config {
        match config::Config::load(config_path) {
            Ok(cfg) => {
                let export = if cli.export != PathBuf::from("/export") {
                    cli.export.clone()
                } else {
                    PathBuf::from(&cfg.export.path)
                };
                let listen = if cli.listen != "0.0.0.0:2049" {
                    cli.listen.clone()
                } else {
                    cfg.server.listen
                };
                (export, listen)
            }
            Err(e) => {
                error!("Failed to load config: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        (cli.export, cli.listen)
    };

    // Validate export path
    let export_path = export_path.canonicalize().unwrap_or_else(|e| {
        error!("Export path {} does not exist: {}", export_path.display(), e);
        std::process::exit(1);
    });

    if !export_path.is_dir() {
        error!("Export path {} is not a directory", export_path.display());
        std::process::exit(1);
    }

    info!(
        export = %export_path.display(),
        listen = %listen_addr,
        "starting nextnfs"
    );

    // Create VFS rooted at the export path
    let root: VfsPath = AltrootFS::new(VfsPath::new(PhysicalFS::new(&export_path))).into();

    let mut builder = NFSServer::builder(root, export_path);
    builder.bind(&listen_addr);
    let server = builder.build();

    // Handle shutdown signals
    let server_task = tokio::spawn(async move {
        server.start_async().await;
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT, shutting down");
        }
        result = server_task => {
            if let Err(e) = result {
                error!("server task failed: {:?}", e);
            }
        }
    }
}
