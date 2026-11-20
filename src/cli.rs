/// CLI client for the nextnfs REST API.
pub async fn export_list(api_url: &str) {
    let url = format!("{}/api/v1/exports", api_url);
    match reqwest::get(&url).await {
        Ok(resp) => {
            if resp.status().is_success() {
                let exports: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                if exports.is_empty() {
                    println!("No exports configured.");
                } else {
                    println!(
                        "{:<16} {:<40} {:<6} ID",
                        "NAME", "PATH", "MODE"
                    );
                    for e in &exports {
                        let mode = if e["read_only"].as_bool().unwrap_or(false) {
                            "RO"
                        } else {
                            "RW"
                        };
                        println!(
                            "{:<16} {:<40} {:<6} {}",
                            e["name"].as_str().unwrap_or(""),
                            e["path"].as_str().unwrap_or(""),
                            mode,
                            e["export_id"].as_u64().unwrap_or(0)
                        );
                    }
                }
            } else {
                eprintln!("Error: HTTP {}", resp.status());
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

/// The API token from `--token-file` (first line, trimmed), for export
/// changes (#122). `None` when no file was given.
fn read_token(token_file: Option<&std::path::Path>) -> Result<Option<String>, String> {
    let Some(f) = token_file else { return Ok(None) };
    let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {}", f.display(), e))?;
    Ok(Some(text.lines().next().unwrap_or("").trim().to_string()))
}

fn with_token(req: reqwest::RequestBuilder, token: Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

pub async fn export_add(
    api_url: &str,
    name: &str,
    path: &str,
    read_only: bool,
    token_file: Option<&std::path::Path>,
) {
    let token = match read_token(token_file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: token file {}", e);
            return;
        }
    };
    let url = format!("{}/api/v1/exports", api_url);
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "name": name,
        "path": path,
        "read_only": read_only,
    });

    match with_token(client.post(&url).json(&body), token).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                println!(
                    "Export '{}' added at {} (id={})",
                    data["name"].as_str().unwrap_or(""),
                    data["path"].as_str().unwrap_or(""),
                    data["export_id"].as_u64().unwrap_or(0)
                );
            } else {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                eprintln!(
                    "Error: {}",
                    data["error"].as_str().unwrap_or("unknown error")
                );
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

pub async fn export_remove(api_url: &str, name: &str, token_file: Option<&std::path::Path>) {
    let token = match read_token(token_file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: token file {}", e);
            return;
        }
    };
    let url = format!("{}/api/v1/exports/{}", api_url, name);
    let client = reqwest::Client::new();

    match with_token(client.delete(&url), token).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                println!("Export '{}' removed.", name);
            } else {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                eprintln!(
                    "Error: {}",
                    data["error"].as_str().unwrap_or("unknown error")
                );
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

pub async fn stats(api_url: &str) {
    let url = format!("{}/api/v1/stats", api_url);
    match reqwest::get(&url).await {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                println!("Server Statistics:");
                println!(
                    "  Total Ops:     {}",
                    data["total_ops"].as_u64().unwrap_or(0)
                );
                println!(
                    "  Total Reads:   {}",
                    data["total_reads"].as_u64().unwrap_or(0)
                );
                println!(
                    "  Total Writes:  {}",
                    data["total_writes"].as_u64().unwrap_or(0)
                );
                println!(
                    "  Bytes Read:    {}",
                    data["total_bytes_read"].as_u64().unwrap_or(0)
                );
                println!(
                    "  Bytes Written: {}",
                    data["total_bytes_written"].as_u64().unwrap_or(0)
                );
                if let Some(exports) = data["exports"].as_array() {
                    for e in exports {
                        println!("\n  Export: {}", e["name"].as_str().unwrap_or(""));
                        if let Some(s) = e.get("stats") {
                            println!(
                                "    Reads: {}  Writes: {}  Ops: {}",
                                s["reads"].as_u64().unwrap_or(0),
                                s["writes"].as_u64().unwrap_or(0),
                                s["ops"].as_u64().unwrap_or(0)
                            );
                        }
                    }
                }
            } else {
                eprintln!("Error: HTTP {}", resp.status());
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

pub async fn health(api_url: &str) {
    let url = format!("{}/health", api_url);
    match reqwest::get(&url).await {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                println!(
                    "Status: {}  Exports: {}",
                    data["status"].as_str().unwrap_or("unknown"),
                    data["exports"].as_u64().unwrap_or(0)
                );
            } else {
                eprintln!("Error: HTTP {}", resp.status());
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
