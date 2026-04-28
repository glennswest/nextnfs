//! nextnfs-stress — POSIX-path stress harness aimed at NFS mount points.
//!
//! Exercises the workload patterns that hit the recent silly-rename / ESTALE
//! and inode-preservation fixes on the server: mass create, GETATTR storms,
//! rename rotation, unlink-while-open (silly rename), and parallel workers.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use rand::Rng;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

#[derive(Parser, Debug)]
#[command(
    name = "nextnfs-stress",
    version,
    about = "Stress test a POSIX path (typically an NFS mount) with parallel file ops"
)]
struct Cli {
    /// Target directory (must exist and be writable). Typically an NFS mount.
    #[arg(short, long)]
    target: PathBuf,

    /// Number of files to create per phase
    #[arg(short, long, default_value_t = 1000)]
    count: usize,

    /// Number of concurrent workers for parallel phases
    #[arg(short = 'j', long, default_value_t = 32)]
    parallel: usize,

    /// File payload size in bytes
    #[arg(short = 's', long, default_value_t = 4096)]
    size: usize,

    /// Skip cleanup at the end (leave files in place for inspection)
    #[arg(long)]
    keep: bool,

    /// Bail out on the first failure rather than continuing through phases
    #[arg(long)]
    fail_fast: bool,
}

#[derive(Default, Debug)]
struct PhaseStats {
    name: &'static str,
    ops: u64,
    bytes: u64,
    errors: HashMap<String, u64>,
    duration_ms: u128,
}

impl PhaseStats {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }

    fn record_err(&mut self, e: &std::io::Error) {
        let key = match e.raw_os_error() {
            Some(c) => format!("{} (errno {c})", e.kind()),
            None => format!("{}", e.kind()),
        };
        *self.errors.entry(key).or_insert(0) += 1;
    }

    fn ops_per_sec(&self) -> f64 {
        if self.duration_ms == 0 {
            0.0
        } else {
            (self.ops as f64) * 1000.0 / (self.duration_ms as f64)
        }
    }

    fn passed(&self) -> bool {
        self.errors.is_empty()
    }

    fn print(&self) {
        let label = if self.passed() {
            "PASS".green().bold()
        } else {
            "FAIL".red().bold()
        };
        println!(
            "  [{label}] {:<28} ops={} bytes={} time={}ms ({:.1} ops/s)",
            self.name,
            self.ops,
            self.bytes,
            self.duration_ms,
            self.ops_per_sec()
        );
        for (k, v) in &self.errors {
            println!("       {} {} × {}", "↳".red(), v, k);
        }
    }
}

fn payload(seed: usize, size: usize) -> Vec<u8> {
    // Deterministic payload so reads can be byte-compared.
    let header = format!("file_{seed:08}\n");
    let mut buf = header.into_bytes();
    buf.resize(size, b'.');
    buf
}

fn fname(i: usize) -> String {
    format!("file_{i:06}")
}

fn rotated(i: usize, n: usize) -> String {
    format!("file_{:06}", (i + 1) % n)
}

async fn phase_create(target: &Path, count: usize, size: usize, parallel: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("create");
    let sem = Arc::new(Semaphore::new(parallel));
    let mut handles = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        let target = target.to_path_buf();
        let sem = sem.clone();
        let h = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let path = target.join(fname(i));
            let buf = payload(i, size);
            let mut f = fs::File::create(&path).await?;
            f.write_all(&buf).await?;
            f.sync_all().await?;
            Ok::<usize, std::io::Error>(buf.len())
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok(n) => {
                stats.ops += 1;
                stats.bytes += n as u64;
            }
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

async fn phase_readdir(target: &Path, expected: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("readdir");
    let start = Instant::now();
    match fs::read_dir(target).await {
        Ok(mut rd) => {
            let mut n = 0u64;
            loop {
                match rd.next_entry().await {
                    Ok(Some(_)) => n += 1,
                    Ok(None) => break,
                    Err(e) => {
                        stats.record_err(&e);
                        break;
                    }
                }
            }
            stats.ops = n;
            if (n as usize) != expected {
                let e = std::io::Error::new(
                    ErrorKind::Other,
                    format!("readdir count mismatch: got {n}, want {expected}"),
                );
                stats.record_err(&e);
            }
        }
        Err(e) => stats.record_err(&e),
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

async fn phase_stat(target: &Path, count: usize, parallel: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("stat");
    let sem = Arc::new(Semaphore::new(parallel));
    let mut handles = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        let target = target.to_path_buf();
        let sem = sem.clone();
        let h = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let path = target.join(fname(i));
            let md = fs::metadata(&path).await?;
            Ok::<u64, std::io::Error>(md.len())
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok(_) => stats.ops += 1,
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

async fn phase_read_verify(
    target: &Path,
    count: usize,
    size: usize,
    parallel: usize,
) -> PhaseStats {
    let mut stats = PhaseStats::new("read+verify");
    let sem = Arc::new(Semaphore::new(parallel));
    let mut handles = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        let target = target.to_path_buf();
        let sem = sem.clone();
        let h = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let path = target.join(fname(i));
            let got = fs::read(&path).await?;
            let want = payload(i, size);
            if got != want {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!("content mismatch on {}", path.display()),
                ));
            }
            Ok::<usize, std::io::Error>(got.len())
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok(n) => {
                stats.ops += 1;
                stats.bytes += n as u64;
            }
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

/// Renames file_N → file_(N+1)%count via two-step swap through a temp name to avoid collisions.
/// We do it in batches with a tmp suffix so the result is a deterministic rotation.
async fn phase_rename_rotate(target: &Path, count: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("rename rotate");
    let start = Instant::now();
    // Stage 1: rename file_N → file_N.tmp
    for i in 0..count {
        let from = target.join(fname(i));
        let to = target.join(format!("{}.tmp", fname(i)));
        if let Err(e) = fs::rename(&from, &to).await {
            stats.record_err(&e);
        } else {
            stats.ops += 1;
        }
    }
    // Stage 2: rename file_N.tmp → file_(N+1)%count
    for i in 0..count {
        let from = target.join(format!("{}.tmp", fname(i)));
        let to = target.join(rotated(i, count));
        if let Err(e) = fs::rename(&from, &to).await {
            stats.record_err(&e);
        } else {
            stats.ops += 1;
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

/// Open a file, unlink it while still open, write more data, then close.
/// This is the classic silly-rename trigger pattern: client sees the unlink,
/// server must keep the inode alive for the open handle.
async fn phase_unlink_while_open(target: &Path, count: usize, parallel: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("unlink-while-open");
    let sem = Arc::new(Semaphore::new(parallel));
    let mut handles = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        let target = target.to_path_buf();
        let sem = sem.clone();
        let h = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let path = target.join(format!("uwo_{i:06}"));
            // Create
            {
                let mut f = fs::File::create(&path).await?;
                f.write_all(b"initial\n").await?;
                f.sync_all().await?;
            }
            // Open, unlink while open, write more, then drop
            let mut f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .await?;
            fs::remove_file(&path).await?;
            f.write_all(b"after-unlink\n").await?;
            f.sync_all().await?;
            // file is now silly-renamed on the server side
            Ok::<(), std::io::Error>(())
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok(()) => stats.ops += 1,
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

/// Parallel workers each create N/W files in their own subdir and then delete them.
async fn phase_parallel_workers(
    target: &Path,
    count: usize,
    workers: usize,
    size: usize,
) -> PhaseStats {
    let mut stats = PhaseStats::new("parallel workers");
    let per_worker = count / workers.max(1);
    let mut handles = Vec::with_capacity(workers);
    let start = Instant::now();
    for w in 0..workers {
        let target = target.to_path_buf();
        let h = tokio::spawn(async move {
            let mut local_ops = 0u64;
            let mut local_bytes = 0u64;
            let dir = target.join(format!("w{w:03}"));
            fs::create_dir_all(&dir).await?;
            for i in 0..per_worker {
                let path = dir.join(format!("p_{i:06}"));
                let buf = payload(w * 1_000 + i, size);
                let mut f = fs::File::create(&path).await?;
                f.write_all(&buf).await?;
                local_ops += 1;
                local_bytes += buf.len() as u64;
            }
            for i in 0..per_worker {
                let path = dir.join(format!("p_{i:06}"));
                fs::remove_file(&path).await?;
                local_ops += 1;
            }
            fs::remove_dir(&dir).await?;
            Ok::<(u64, u64), std::io::Error>((local_ops, local_bytes))
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok((ops, bytes)) => {
                stats.ops += ops;
                stats.bytes += bytes;
            }
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

async fn phase_delete(target: &Path, count: usize, parallel: usize) -> PhaseStats {
    let mut stats = PhaseStats::new("delete");
    let sem = Arc::new(Semaphore::new(parallel));
    let mut handles = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        let target = target.to_path_buf();
        let sem = sem.clone();
        let h = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            // After rotate phase, the original i-th file content is now at rotated(i,count).
            // But for cleanup we just remove fname(i) — which exists because rotation is a permutation.
            let path = target.join(fname(i));
            fs::remove_file(&path).await?;
            Ok::<(), std::io::Error>(())
        });
        handles.push(h);
    }
    for h in handles {
        match h.await.expect("join") {
            Ok(()) => stats.ops += 1,
            Err(e) => stats.record_err(&e),
        }
    }
    stats.duration_ms = start.elapsed().as_millis();
    stats
}

async fn run(cli: Cli) -> Result<bool> {
    let md = fs::metadata(&cli.target)
        .await
        .with_context(|| format!("target {} is not accessible", cli.target.display()))?;
    if !md.is_dir() {
        bail!("target {} is not a directory", cli.target.display());
    }

    // Use a unique workdir under the target to avoid clobbering anything.
    let suffix: u64 = rand::thread_rng().gen();
    let work = cli.target.join(format!("stress_{suffix:016x}"));
    fs::create_dir(&work)
        .await
        .with_context(|| format!("cannot create workdir {}", work.display()))?;
    println!(
        "{} target={} count={} parallel={} size={}B work={}",
        "stress run".bold(),
        cli.target.display(),
        cli.count,
        cli.parallel,
        cli.size,
        work.display()
    );

    let mut all_pass = true;
    let mut totals: Vec<PhaseStats> = Vec::new();

    macro_rules! run_phase {
        ($e:expr) => {{
            let s = $e.await;
            s.print();
            let pass = s.passed();
            totals.push(s);
            if !pass {
                all_pass = false;
                if cli.fail_fast {
                    return finalise(&work, totals, all_pass, cli.keep).await;
                }
            }
        }};
    }

    run_phase!(phase_create(&work, cli.count, cli.size, cli.parallel));
    run_phase!(phase_readdir(&work, cli.count));
    run_phase!(phase_stat(&work, cli.count, cli.parallel));
    run_phase!(phase_read_verify(&work, cli.count, cli.size, cli.parallel));
    run_phase!(phase_rename_rotate(&work, cli.count));
    // After rotate, names are still {file_000000..file_(count-1)}, just permuted.
    run_phase!(phase_stat(&work, cli.count, cli.parallel));
    run_phase!(phase_unlink_while_open(&work, cli.count, cli.parallel));
    run_phase!(phase_parallel_workers(
        &work,
        cli.count,
        cli.parallel,
        cli.size
    ));
    run_phase!(phase_delete(&work, cli.count, cli.parallel));

    finalise(&work, totals, all_pass, cli.keep).await
}

async fn finalise(
    work: &Path,
    totals: Vec<PhaseStats>,
    all_pass: bool,
    keep: bool,
) -> Result<bool> {
    if !keep {
        // Best-effort cleanup of the workdir
        let _ = fs::remove_dir_all(work).await;
    }
    println!();
    println!("{}", "summary".bold());
    let total_ops: u64 = totals.iter().map(|s| s.ops).sum();
    let total_bytes: u64 = totals.iter().map(|s| s.bytes).sum();
    let total_ms: u128 = totals.iter().map(|s| s.duration_ms).sum();
    let total_errors: u64 = totals
        .iter()
        .map(|s| s.errors.values().sum::<u64>())
        .sum();
    println!(
        "  ops={} bytes={} time={}ms errors={}",
        total_ops, total_bytes, total_ms, total_errors
    );
    if all_pass {
        println!("  result: {}", "PASS".green().bold());
    } else {
        println!("  result: {}", "FAIL".red().bold());
    }
    Ok(all_pass)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();
    let pass = run(cli).await?;
    if !pass {
        std::process::exit(1);
    }
    Ok(())
}
