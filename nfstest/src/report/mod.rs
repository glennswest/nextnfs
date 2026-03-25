use crate::harness::{RunResult, TestStatus};
use colored::Colorize;
use std::path::Path;

/// Write the run result as a JSON file.
pub fn write_json_report(result: &RunResult) -> anyhow::Result<()> {
    let filename = format!("report-{}.json", result.run_id);
    let path = result.output_dir.join(&filename);
    let json = serde_json::to_string_pretty(result)?;
    std::fs::write(&path, json)?;
    tracing::info!("JSON report written to {}", path.display());
    Ok(())
}

/// Write the run result as a Markdown file.
pub fn write_markdown_report(result: &RunResult) -> anyhow::Result<()> {
    let filename = format!("report-{}.md", result.run_id);
    let path = result.output_dir.join(&filename);

    let mut md = String::new();

    md.push_str(&format!("# NFS Test Report\n\n"));
    md.push_str(&format!("**Run ID:** `{}`\n\n", result.run_id));
    md.push_str(&format!(
        "**Server:** `{}:{}{}`\n\n",
        result.server, result.port, result.export
    ));
    md.push_str(&format!("**Started:** {}\n\n", result.started_at));
    md.push_str(&format!("**Finished:** {}\n\n", result.finished_at));
    md.push_str(&format!(
        "**Duration:** {:.2}s\n\n",
        result.summary.duration.as_secs_f64()
    ));

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Count |\n");
    md.push_str("|--------|-------|\n");
    md.push_str(&format!("| Total | {} |\n", result.summary.total));
    md.push_str(&format!("| Passed | {} |\n", result.summary.passed));
    md.push_str(&format!("| Failed | {} |\n", result.summary.failed));
    md.push_str(&format!("| Skipped | {} |\n", result.summary.skipped));
    md.push_str(&format!("| Errors | {} |\n", result.summary.errors));
    md.push_str("\n");

    // Pass rate
    let pass_rate = if result.summary.total > 0 {
        (result.summary.passed as f64 / result.summary.total as f64) * 100.0
    } else {
        0.0
    };
    md.push_str(&format!("**Pass rate:** {:.1}%\n\n", pass_rate));

    // Version breakdown
    md.push_str("## Results by Version\n\n");
    for version in &[
        crate::harness::NfsVersion::V3,
        crate::harness::NfsVersion::V4_0,
        crate::harness::NfsVersion::V4_1,
        crate::harness::NfsVersion::V4_2,
    ] {
        let version_results: Vec<_> = result.results.iter().filter(|r| r.version == *version).collect();
        if version_results.is_empty() {
            continue;
        }

        let passed = version_results.iter().filter(|r| r.status == TestStatus::Pass).count();
        let total = version_results.len();

        md.push_str(&format!("### {} ({}/{})\n\n", version, passed, total));
        md.push_str("| Test ID | Description | Status | Duration | Message |\n");
        md.push_str("|---------|-------------|--------|----------|---------|\n");

        for r in &version_results {
            let status_icon = match r.status {
                TestStatus::Pass => "PASS",
                TestStatus::Fail => "FAIL",
                TestStatus::Skip => "SKIP",
                TestStatus::Error => "ERROR",
            };
            let msg = r.message.as_deref().unwrap_or("");
            md.push_str(&format!(
                "| {} | {} | {} | {:.3}s | {} |\n",
                r.id,
                r.description,
                status_icon,
                r.duration.as_secs_f64(),
                msg
            ));
        }
        md.push_str("\n");
    }

    // Failed tests detail
    let failures: Vec<_> = result
        .results
        .iter()
        .filter(|r| r.status == TestStatus::Fail || r.status == TestStatus::Error)
        .collect();

    if !failures.is_empty() {
        md.push_str("## Failures\n\n");
        for r in &failures {
            md.push_str(&format!("### {} — {}\n\n", r.id, r.description));
            md.push_str(&format!("- **Status:** {}\n", r.status));
            md.push_str(&format!("- **Version:** {}\n", r.version));
            if let Some(ref msg) = r.message {
                md.push_str(&format!("- **Message:** {}\n", msg));
            }
            if let Some(ref detail) = r.detail {
                md.push_str(&format!("\n```\n{}\n```\n", detail));
            }
            md.push_str("\n");
        }
    }

    std::fs::write(&path, md)?;
    tracing::info!("Markdown report written to {}", path.display());
    Ok(())
}

/// Print a summary to stdout.
pub fn print_summary(result: &RunResult) {
    println!();
    println!("{}", "═══════════════════════════════════════".bold());
    println!("{}", "  Test Run Summary".bold());
    println!("{}", "═══════════════════════════════════════".bold());
    println!("  Run ID:   {}", result.run_id);
    println!("  Server:   {}:{}{}", result.server, result.port, result.export);
    println!(
        "  Duration: {:.2}s",
        result.summary.duration.as_secs_f64()
    );
    println!();
    println!(
        "  Total:    {}",
        result.summary.total.to_string().bold()
    );
    println!(
        "  Passed:   {}",
        result.summary.passed.to_string().green().bold()
    );
    println!(
        "  Failed:   {}",
        if result.summary.failed > 0 {
            result.summary.failed.to_string().red().bold()
        } else {
            result.summary.failed.to_string().green().bold()
        }
    );
    println!(
        "  Skipped:  {}",
        result.summary.skipped.to_string().yellow()
    );
    println!(
        "  Errors:   {}",
        if result.summary.errors > 0 {
            result.summary.errors.to_string().red()
        } else {
            result.summary.errors.to_string().normal()
        }
    );
    println!("{}", "═══════════════════════════════════════".bold());

    let pass_rate = if result.summary.total > 0 {
        (result.summary.passed as f64 / result.summary.total as f64) * 100.0
    } else {
        0.0
    };
    println!("  Pass rate: {:.1}%", pass_rate);

    println!();
    println!(
        "  Reports: {}/report-{}.json",
        result.output_dir.display(),
        result.run_id
    );
    println!(
        "           {}/report-{}.md",
        result.output_dir.display(),
        result.run_id
    );
    println!();
}

/// Show a previously saved report.
pub fn show_report(path: &Path, format: &str) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let result: RunResult = serde_json::from_str(&content)?;

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "markdown" | "md" => {
            // Re-generate markdown to stdout
            write_markdown_to_stdout(&result);
        }
        _ => {
            print_summary(&result);

            // Also print failures
            let failures: Vec<_> = result
                .results
                .iter()
                .filter(|r| r.status == TestStatus::Fail || r.status == TestStatus::Error)
                .collect();

            if !failures.is_empty() {
                println!("  {}", "Failures:".red().bold());
                for r in &failures {
                    println!(
                        "    {} {} — {}",
                        r.id.bold(),
                        r.status,
                        r.message.as_deref().unwrap_or("(no message)")
                    );
                }
                println!();
            }
        }
    }

    Ok(())
}

fn write_markdown_to_stdout(result: &RunResult) {
    println!("# NFS Test Report\n");
    println!("**Run ID:** `{}`\n", result.run_id);
    println!(
        "**Server:** `{}:{}{}`\n",
        result.server, result.port, result.export
    );
    println!("**Started:** {}\n", result.started_at);
    println!(
        "**Duration:** {:.2}s\n",
        result.summary.duration.as_secs_f64()
    );
    println!("## Summary\n");
    println!(
        "| Total | Passed | Failed | Skipped | Errors |"
    );
    println!("|-------|--------|--------|---------|--------|");
    println!(
        "| {} | {} | {} | {} | {} |",
        result.summary.total,
        result.summary.passed,
        result.summary.failed,
        result.summary.skipped,
        result.summary.errors
    );
}
