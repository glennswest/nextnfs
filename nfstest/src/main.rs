#![allow(unused_variables)]

mod harness;
mod report;
pub mod web;
mod wire;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nextnfstest", version, about = "NFS protocol test suite")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the full test suite or a filtered subset
    Run {
        /// NFS server hostname or IP
        #[arg(short, long)]
        server: String,

        /// NFS server port (default 2049)
        #[arg(short, long, default_value = "2049")]
        port: u16,

        /// Export path on the server
        #[arg(short, long, default_value = "/")]
        export: String,

        /// NFS version filter: 3, 4.0, 4.1, 4.2, or "all"
        #[arg(short = 'V', long, default_value = "all")]
        version: String,

        /// Test layer filter: wire, functional, interop, stress, perf, or "all"
        #[arg(short, long, default_value = "all")]
        layer: String,

        /// Tag filter (e.g., "smoke", "ci", "nightly")
        #[arg(short, long)]
        tag: Option<String>,

        /// Specific test ID to run (e.g., "W3-001")
        #[arg(short = 'i', long)]
        test_id: Option<String>,

        /// Output directory for test reports
        #[arg(short, long, default_value = "reports")]
        output: PathBuf,

        /// AUTH_SYS UID to use
        #[arg(long, default_value = "0")]
        uid: u32,

        /// AUTH_SYS GID to use
        #[arg(long, default_value = "0")]
        gid: u32,
    },

    /// List all available tests
    List {
        /// NFS version filter
        #[arg(short = 'V', long, default_value = "all")]
        version: String,

        /// Test layer filter
        #[arg(short, long, default_value = "all")]
        layer: String,

        /// Tag filter
        #[arg(short, long)]
        tag: Option<String>,
    },

    /// Show results from a previous test run
    Report {
        /// Path to the test report JSON file
        #[arg(short, long)]
        file: PathBuf,

        /// Output format: text, json, markdown
        #[arg(short = 'F', long, default_value = "text")]
        format: String,
    },

    /// Start the web UI server
    Serve {
        /// Address to bind to
        #[arg(short, long, default_value = "0.0.0.0:3000")]
        bind: String,

        /// Directory for storing test reports
        #[arg(short, long, default_value = "/data/reports")]
        data_dir: PathBuf,

        /// Base path for reverse proxy (e.g., /ui/proxy/nextnfstest/)
        #[arg(long, default_value = "/")]
        base_path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            server,
            port,
            export,
            version,
            layer,
            tag,
            test_id,
            output,
            uid,
            gid,
        } => {
            let config = harness::RunConfig {
                server,
                port,
                export,
                version_filter: harness::parse_version_filter(&version),
                layer_filter: harness::parse_layer_filter(&layer),
                tag_filter: tag,
                test_id_filter: test_id,
                output_dir: output,
                uid,
                gid,
            };

            let manager = harness::TestManager::new(config);
            let run_result = manager.run().await?;

            // Generate reports
            report::write_json_report(&run_result)?;
            report::write_markdown_report(&run_result)?;
            report::print_summary(&run_result);

            // Exit with non-zero if any tests failed
            if run_result.summary.failed > 0 {
                std::process::exit(1);
            }
        }

        Commands::List {
            version,
            layer,
            tag,
        } => {
            let version_filter = harness::parse_version_filter(&version);
            let layer_filter = harness::parse_layer_filter(&layer);
            harness::list_tests(&version_filter, &layer_filter, &tag);
        }

        Commands::Report { file, format } => {
            report::show_report(&file, &format)?;
        }

        Commands::Serve {
            bind,
            data_dir,
            base_path,
        } => {
            web::serve(&bind, data_dir, base_path).await?;
        }
    }

    Ok(())
}
