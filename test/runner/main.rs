//! nextnfs's test image entry point, `/test <suite>` (stormcentral
//! docs/test-standard.md; #133).
//!
//! The image carries `/nextnfs-server-tests`, the nextnfs-server library's
//! own test binary (built by test/build.sh at the commit under test). This
//! program runs the tests a suite names in it, one at a time, as root in a
//! privileged pod on a test machine, which is what the build VMs cannot do:
//! the kernel file handle tests resolve handles with `open_by_handle_at(2)`,
//! which needs CAP_DAC_READ_SEARCH. `NEXTNFS_REQUIRE_PRIVILEGED` turns their
//! "not permitted / no kernel handles" early returns into failures, so a run
//! cannot pass without having exercised that path.
//!
//! Suites:
//! - `short`: the kernel handle tests (`kernel_handle`): handles survive a
//!   server restart, follow a rename made outside NFS, a recreated file's
//!   handle is STALE, and handles of objects outside the export or removed
//!   do not resolve.
//! - `medium`: those, plus the export confinement tests (`beneath::`) and
//!   the project quota tests (`quota::`), as root.
//! - `long`: `medium` repeated in waves until the run's budget is nearly
//!   used; a wave that fails, or that takes more than three times the
//!   first wave, fails the suite.
//!
//! Output: one JSON object per test on stdout, then a summary; exit 0 all
//! passed, 1 a test failed, 2 the suite could not run. Each test's output is
//! kept under $STORM_RESULTS/logs/. Scratch files go under
//! $STORM_RESULTS/scratch (removed at the end): the image is FROM scratch.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const TESTS: &str = "/nextnfs-server-tests";

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn emit(test: &str, status: &str, ms: u128, detail: &str) {
    println!(
        "{{\"test\": {}, \"status\": {}, \"ms\": {}, \"detail\": {}}}",
        json_str(test),
        json_str(status),
        ms,
        json_str(detail)
    );
    let _ = std::io::stdout().flush();
}

/// Whether this process has CAP_DAC_READ_SEARCH (bit 2 of CapEff).
fn has_dac_read_search() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else { return false };
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << 2) != 0)
}

/// Every test in the binary whose name contains one of `filters`.
fn list(filters: &[&str]) -> Result<Vec<String>, String> {
    let out = Command::new(TESTS)
        .args(["--list", "--format", "terse"])
        .output()
        .map_err(|e| format!("{TESTS}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{TESTS} --list: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .filter(|name| filters.iter().any(|f| name.contains(f)))
        .map(str::to_string)
        .collect())
}

struct Outcome {
    pass: u32,
    fail: u32,
}

/// Run each test alone; report it; keep its output.
fn run_tests(names: &[String], scratch: &Path, logs: &Path, wave: Option<u32>) -> Outcome {
    let mut o = Outcome { pass: 0, fail: 0 };
    for name in names {
        let started = Instant::now();
        let result = Command::new(TESTS)
            .args(["--exact", name, "--test-threads=1", "--nocapture"])
            .env("TMPDIR", scratch)
            .env("NEXTNFS_REQUIRE_PRIVILEGED", "1")
            .env("RUST_BACKTRACE", "1")
            .output();
        let ms = started.elapsed().as_millis();
        let label = match wave {
            Some(w) => format!("wave{w}:{name}"),
            None => name.clone(),
        };
        let (ok, text) = match result {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                // libtest exits 0 with "0 passed" when the name matched
                // nothing: that is not a pass.
                let ran = text.contains("1 passed");
                (out.status.success() && ran, text)
            }
            Err(e) => (false, format!("{TESTS}: {e}")),
        };
        let _ = std::fs::write(logs.join(format!("{}.log", label.replace([':', '/'], "_"))), &text);
        let detail: String = if ok {
            String::new()
        } else {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            lines[lines.len().saturating_sub(12)..].join("\n")
        };
        if ok {
            o.pass += 1;
            emit(&label, "pass", ms, &detail);
        } else {
            o.fail += 1;
            emit(&label, "fail", ms, &detail);
        }
    }
    o
}

fn main() -> ExitCode {
    let suite = std::env::args().nth(1).unwrap_or_else(|| "short".to_string());
    let filters: &[&str] = match suite.as_str() {
        "short" => &["kernel_handle"],
        "medium" | "long" => &["kernel_handle", "beneath::", "quota::"],
        other => {
            eprintln!("unknown suite {other:?} (short, medium, long)");
            return ExitCode::from(2);
        }
    };
    let results = PathBuf::from(std::env::var("STORM_RESULTS").unwrap_or_else(|_| "/results".into()));
    let scratch = results.join("scratch");
    let logs = results.join("logs");
    for d in [&scratch, &logs] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("{}: {e}", d.display());
            return ExitCode::from(2);
        }
    }
    let infra = |msg: &str| {
        eprintln!("{msg}");
        println!("{{\"summary\": {{\"pass\": 0, \"fail\": 0, \"skip\": 0}}}}");
        ExitCode::from(2)
    };
    if !has_dac_read_search() {
        return infra("no CAP_DAC_READ_SEARCH: the suite needs the privileged pod test/requires.toml declares");
    }
    let names = match list(filters) {
        Ok(n) if !n.is_empty() => n,
        Ok(_) => return infra(&format!("no tests in {TESTS} match {filters:?}")),
        Err(e) => return infra(&e),
    };

    let (pass, fail) = if suite == "long" {
        // Waves until the budget is nearly used (the standard's deadline is
        // STORM_TIMEOUT; leave a fifth of it, at least two minutes).
        let budget = std::env::var("STORM_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(8 * 3600);
        let stop = Duration::from_secs(budget - (budget / 5).max(120).min(budget));
        let started = Instant::now();
        let (mut pass, mut fail) = (0, 0);
        let mut first: Option<Duration> = None;
        let mut wave = 1;
        loop {
            let t = Instant::now();
            let o = run_tests(&names, &scratch, &logs, Some(wave));
            let took = t.elapsed();
            pass += o.pass;
            fail += o.fail;
            let base = *first.get_or_insert(took);
            let slow = took > base * 3 && took > Duration::from_secs(5);
            emit(
                &format!("wave{wave}"),
                if o.fail == 0 && !slow { "pass" } else { "fail" },
                took.as_millis(),
                &format!("{} tests, first wave {} ms", names.len(), base.as_millis()),
            );
            if slow || o.fail > 0 {
                fail += 1;
                break;
            }
            pass += 1;
            if started.elapsed() + took * 2 > stop {
                break;
            }
            wave += 1;
        }
        (pass, fail)
    } else {
        let o = run_tests(&names, &scratch, &logs, None);
        (o.pass, o.fail)
    };
    let _ = std::fs::remove_dir_all(&scratch);
    println!("{{\"summary\": {{\"pass\": {pass}, \"fail\": {fail}, \"skip\": 0}}}}");
    if fail == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
