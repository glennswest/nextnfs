use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::Html;

use super::css::CSS;
use super::AppState;
use crate::harness::{RunResult, TestStatus};

fn layout(base_path: &str, title: &str, active: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<base href="{base_path}">
<title>{title} — nextnfstest</title>
<style>{CSS}</style>
</head>
<body>
<nav>
  <span class="brand">nextnfstest</span>
  <a href="./" class="{da}">Dashboard</a>
  <a href="run" class="{ra}">Run Tests</a>
  <a href="results" class="{rea}">Results</a>
</nav>
{body}
</body>
</html>"#,
        base_path = base_path,
        title = title,
        CSS = CSS,
        body = body,
        da = if active == "dashboard" { "active" } else { "" },
        ra = if active == "run" { "active" } else { "" },
        rea = if active == "results" { "active" } else { "" },
    )
}

pub async fn dashboard(State(state): State<Arc<AppState>>) -> Html<String> {
    let active = state.active_run.read().await;
    let status_html = if let Some(ref run) = *active {
        format!(
            r#"<div class="card" style="border-color:var(--purple)">
  <div class="header-row">
    <h3>Run in progress</h3>
    <span class="badge badge-running">RUNNING</span>
  </div>
  <p class="text-muted text-sm mt-16">Run ID: <span class="mono">{run_id}</span></p>
  <div class="progress-bar mt-16"><div class="progress-fill" id="progress-fill" style="width:{pct}%"></div></div>
  <p class="text-sm text-muted" id="progress-text">{completed}/{total} tests</p>
  <table id="live-table" class="mt-16">
    <thead><tr><th>Test</th><th>Description</th><th>Status</th><th>Duration</th></tr></thead>
    <tbody id="live-body"></tbody>
  </table>
</div>"#,
            run_id = run.run_id,
            completed = run.completed,
            total = run.total_tests,
            pct = if run.total_tests > 0 {
                run.completed * 100 / run.total_tests
            } else {
                0
            },
        )
    } else {
        String::new()
    };
    let is_running = active.is_some();
    drop(active);

    // Count past results
    let result_count = std::fs::read_dir(&state.data_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("report-") && n.ends_with(".json"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);

    let test_count = crate::wire::registry().len();

    let body = format!(
        r#"<div class="container">
  <h1>NFS Protocol Test Suite</h1>
  <p class="text-muted mb-16">Wire-level NFS protocol conformance testing (v3, v4.0, v4.1, v4.2)</p>
  {status_html}
  <div class="cards">
    <a href="run" class="card">
      <div class="stat">&rarr;</div>
      <h3>Run Tests</h3>
      <p>Execute the test suite against an NFS server</p>
    </a>
    <a href="results" class="card">
      <div class="stat">{result_count}</div>
      <h3>Past Results</h3>
      <p>View and compare previous test runs</p>
    </a>
    <div class="card">
      <div class="stat">{test_count}</div>
      <h3>Test Registry</h3>
      <p>Wire-level protocol tests across 4 NFS versions</p>
    </div>
  </div>
</div>
<script>
(function() {{
  var running = {is_running};
  if (!running) {{
    // Poll status to detect new runs
    setInterval(function() {{
      fetch('api/status').then(r=>r.json()).then(d=>{{
        if(d.status==='running') location.reload();
      }});
    }}, 3000);
    return;
  }}
  // SSE for live progress
  var es = new EventSource('api/progress');
  es.onmessage = function(e) {{
    var ev = JSON.parse(e.data);
    if (ev.type === 'TestCompleted') {{
      var fill = document.getElementById('progress-fill');
      var txt = document.getElementById('progress-text');
      var pct = Math.round((ev.index + 1) / ev.total * 100);
      fill.style.width = pct + '%';
      txt.textContent = (ev.index + 1) + '/' + ev.total + ' tests';
      var r = ev.result;
      var cls = r.status === 'Pass' ? 'pass' : r.status === 'Fail' ? 'fail' : r.status === 'Skip' ? 'skip' : 'error';
      var row = '<tr><td class="mono">' + r.id + '</td><td>' + r.description + '</td><td class="' + cls + '">' + r.status.toUpperCase() + '</td><td class="mono">' + (r.duration.secs || 0) + '.' + String(Math.floor((r.duration.nanos||0)/1e6)).padStart(3,'0') + 's</td></tr>';
      document.getElementById('live-body').insertAdjacentHTML('beforeend', row);
    }} else if (ev.type === 'RunFinished') {{
      setTimeout(function(){{ location.reload(); }}, 500);
    }}
  }};
}})();
</script>"#,
        status_html = status_html,
        result_count = result_count,
        test_count = test_count,
        is_running = if is_running { "true" } else { "false" },
    );

    Html(layout(&state.base_path, "Dashboard", "dashboard", &body))
}

pub async fn run_form(State(state): State<Arc<AppState>>) -> Html<String> {
    let body = r#"<div class="container">
  <a href="./" class="back-link">&larr; Dashboard</a>
  <h1>Run Tests</h1>
  <p class="text-muted mb-16">Configure and start a test run against an NFS server</p>
  <form id="run-form" style="max-width:640px">
    <div class="form-group">
      <label>NFS Server</label>
      <input type="text" name="server" required placeholder="192.168.1.10 or hostname">
    </div>
    <div class="form-row">
      <div class="form-group">
        <label>Port</label>
        <input type="number" name="port" value="2049">
      </div>
      <div class="form-group">
        <label>Export Path</label>
        <input type="text" name="export" value="/">
      </div>
    </div>
    <div class="form-row">
      <div class="form-group">
        <label>NFS Version</label>
        <select name="version">
          <option value="all">All versions</option>
          <option value="3">NFSv3</option>
          <option value="4.0">NFSv4.0</option>
          <option value="4.1">NFSv4.1</option>
          <option value="4.2">NFSv4.2</option>
        </select>
      </div>
      <div class="form-group">
        <label>Test Layer</label>
        <select name="layer">
          <option value="wire">Wire (protocol-level)</option>
          <option value="all">All layers</option>
          <option value="functional">Functional</option>
          <option value="interop">Interop</option>
          <option value="stress">Stress</option>
          <option value="perf">Performance</option>
        </select>
      </div>
    </div>
    <div class="form-row-3">
      <div class="form-group">
        <label>Tag Filter</label>
        <input type="text" name="tag" placeholder="e.g. smoke, ci">
      </div>
      <div class="form-group">
        <label>UID</label>
        <input type="number" name="uid" value="0">
      </div>
      <div class="form-group">
        <label>GID</label>
        <input type="number" name="gid" value="0">
      </div>
    </div>
    <div class="form-group">
      <label>Specific Test ID</label>
      <input type="text" name="test_id" placeholder="e.g. W3-001 (leave blank for all)">
    </div>
    <button type="submit" class="btn btn-primary" id="submit-btn">Start Test Run</button>
    <span id="run-status" class="text-sm text-muted" style="margin-left:16px"></span>
  </form>
</div>
<script>
document.getElementById('run-form').addEventListener('submit', async function(e) {
  e.preventDefault();
  var btn = document.getElementById('submit-btn');
  var status = document.getElementById('run-status');
  btn.disabled = true;
  status.textContent = 'Starting...';
  var fd = new FormData(e.target);
  var body = {};
  body.server = fd.get('server');
  body.port = parseInt(fd.get('port')) || 2049;
  body.export = fd.get('export') || '/';
  body.version = fd.get('version') || 'all';
  body.layer = fd.get('layer') || 'wire';
  body.tag = fd.get('tag') || null;
  body.test_id = fd.get('test_id') || null;
  body.uid = parseInt(fd.get('uid')) || 0;
  body.gid = parseInt(fd.get('gid')) || 0;
  if (!body.tag) body.tag = null;
  if (!body.test_id) body.test_id = null;
  try {
    var resp = await fetch('api/run', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(body),
    });
    if (resp.ok) {
      window.location.href = './';
    } else if (resp.status === 409) {
      status.textContent = 'A test run is already in progress';
      status.style.color = 'var(--yellow)';
      btn.disabled = false;
    } else {
      var text = await resp.text();
      status.textContent = 'Error: ' + text;
      status.style.color = 'var(--red)';
      btn.disabled = false;
    }
  } catch(err) {
    status.textContent = 'Network error: ' + err.message;
    status.style.color = 'var(--red)';
    btn.disabled = false;
  }
});
</script>"#;

    Html(layout(&state.base_path, "Run Tests", "run", body))
}

pub async fn results_list(State(state): State<Arc<AppState>>) -> Html<String> {
    let mut runs: Vec<RunResult> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&state.data_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_str().unwrap_or("");
            if name.starts_with("report-") && name.ends_with(".json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(result) = serde_json::from_str::<RunResult>(&content) {
                        runs.push(result);
                    }
                }
            }
        }
    }

    // Sort newest first
    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    let rows = if runs.is_empty() {
        r#"<tr><td colspan="7" class="empty">No test runs yet. <a href="run">Run your first test</a>.</td></tr>"#.to_string()
    } else {
        runs.iter()
            .map(|r| {
                let pass_rate = if r.summary.total > 0 {
                    r.summary.passed * 100 / r.summary.total
                } else {
                    0
                };
                let date = r.started_at.format("%Y-%m-%d %H:%M");
                format!(
                    r#"<tr onclick="location.href='results/{run_id}'" style="cursor:pointer">
  <td class="mono">{short_id}</td>
  <td>{date}</td>
  <td>{server}:{port}{export}</td>
  <td><span class="pass">{passed}</span> / <span class="fail">{failed}</span> / <span class="skip">{skipped}</span></td>
  <td>{total}</td>
  <td>{pass_rate}%</td>
  <td><button class="btn btn-danger btn-sm" onclick="event.stopPropagation();deleteRun('{run_id}')">Delete</button></td>
</tr>"#,
                    run_id = r.run_id,
                    short_id = &r.run_id[..8],
                    date = date,
                    server = r.server,
                    port = r.port,
                    export = r.export,
                    passed = r.summary.passed,
                    failed = r.summary.failed,
                    skipped = r.summary.skipped,
                    total = r.summary.total,
                    pass_rate = pass_rate,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let body = format!(
        r#"<div class="container">
  <a href="./" class="back-link">&larr; Dashboard</a>
  <div class="header-row">
    <h1>Test Results</h1>
    <span class="text-muted text-sm">{count} run{s}</span>
  </div>
  <table>
    <thead>
      <tr><th>Run</th><th>Date</th><th>Server</th><th>Pass/Fail/Skip</th><th>Total</th><th>Rate</th><th></th></tr>
    </thead>
    <tbody>{rows}</tbody>
  </table>
</div>
<script>
async function deleteRun(id) {{
  if (!confirm('Delete this run?')) return;
  var resp = await fetch('api/runs/' + id, {{method:'DELETE'}});
  if (resp.ok) location.reload();
  else alert('Failed to delete: ' + await resp.text());
}}
</script>"#,
        count = runs.len(),
        s = if runs.len() == 1 { "" } else { "s" },
        rows = rows,
    );

    Html(layout(&state.base_path, "Results", "results", &body))
}

pub async fn result_detail(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Html<String> {
    let path = state.data_dir.join(format!("report-{run_id}.json"));
    let result = match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<RunResult>(&content) {
            Ok(r) => r,
            Err(_) => return Html(layout(&state.base_path, "Not Found", "results", "<div class=\"container\"><p>Invalid report file.</p></div>")),
        },
        Err(_) => return Html(layout(&state.base_path, "Not Found", "results", "<div class=\"container\"><p>Run not found.</p></div>")),
    };

    let pass_rate = if result.summary.total > 0 {
        result.summary.passed as f64 / result.summary.total as f64 * 100.0
    } else {
        0.0
    };

    let rows: String = result
        .results
        .iter()
        .map(|r| {
            let (cls, status_label) = match r.status {
                TestStatus::Pass => ("pass", "PASS"),
                TestStatus::Fail => ("fail", "FAIL"),
                TestStatus::Skip => ("skip", "SKIP"),
                TestStatus::Error => ("error", "ERROR"),
            };
            let msg = r.message.as_deref().unwrap_or("");
            let dur = format!("{:.3}s", r.duration.as_secs_f64());
            format!(
                r#"<tr class="test-row" data-status="{status_label}">
  <td class="mono">{id}</td>
  <td>{version}</td>
  <td>{desc}</td>
  <td class="{cls}">{status_label}</td>
  <td class="mono">{dur}</td>
  <td class="text-muted text-sm">{msg}</td>
</tr>"#,
                id = r.id,
                version = r.version,
                desc = r.description,
                cls = cls,
                status_label = status_label,
                dur = dur,
                msg = msg,
            )
        })
        .collect();

    let body = format!(
        r#"<div class="container">
  <a href="results" class="back-link">&larr; All Results</a>
  <div class="header-row">
    <div>
      <h1>Run {short_id}</h1>
      <p class="text-muted text-sm">{server}:{port}{export} &mdash; {date}</p>
    </div>
    <button class="btn btn-danger btn-sm" onclick="deleteRun('{run_id}')">Delete Run</button>
  </div>

  <div class="stats mt-24">
    <div class="stat-box"><div class="num" style="color:var(--fg)">{total}</div><div class="label">Total</div></div>
    <div class="stat-box"><div class="num pass">{passed}</div><div class="label">Passed</div></div>
    <div class="stat-box"><div class="num fail">{failed}</div><div class="label">Failed</div></div>
    <div class="stat-box"><div class="num skip">{skipped}</div><div class="label">Skipped</div></div>
    <div class="stat-box"><div class="num error">{errors}</div><div class="label">Errors</div></div>
    <div class="stat-box"><div class="num" style="color:var(--cyan)">{pass_rate:.1}%</div><div class="label">Pass Rate</div></div>
    <div class="stat-box"><div class="num" style="color:var(--muted)">{duration:.2}s</div><div class="label">Duration</div></div>
  </div>

  <div class="progress-bar mt-16">
    <div class="progress-fill" style="width:{pass_rate:.1}%"></div>
  </div>

  <div class="filters mt-24">
    <button class="filter-btn active" onclick="filterTests('all')">All ({total})</button>
    <button class="filter-btn" onclick="filterTests('PASS')">Pass ({passed})</button>
    <button class="filter-btn" onclick="filterTests('FAIL')">Fail ({failed})</button>
    <button class="filter-btn" onclick="filterTests('SKIP')">Skip ({skipped})</button>
    <button class="filter-btn" onclick="filterTests('ERROR')">Error ({errors})</button>
  </div>

  <table>
    <thead>
      <tr><th>Test ID</th><th>Version</th><th>Description</th><th>Status</th><th>Duration</th><th>Message</th></tr>
    </thead>
    <tbody>{rows}</tbody>
  </table>
</div>
<script>
function filterTests(status) {{
  document.querySelectorAll('.filter-btn').forEach(function(b){{ b.classList.remove('active'); }});
  event.target.classList.add('active');
  document.querySelectorAll('.test-row').forEach(function(r){{
    r.style.display = (status === 'all' || r.dataset.status === status) ? '' : 'none';
  }});
}}
async function deleteRun(id) {{
  if (!confirm('Delete this run?')) return;
  var resp = await fetch('api/runs/' + id, {{method:'DELETE'}});
  if (resp.ok) window.location.href = 'results';
  else alert('Failed to delete: ' + await resp.text());
}}
</script>"#,
        short_id = &result.run_id[..8.min(result.run_id.len())],
        run_id = result.run_id,
        server = result.server,
        port = result.port,
        export = result.export,
        date = result.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
        total = result.summary.total,
        passed = result.summary.passed,
        failed = result.summary.failed,
        skipped = result.summary.skipped,
        errors = result.summary.errors,
        pass_rate = pass_rate,
        duration = result.summary.duration.as_secs_f64(),
        rows = rows,
    );

    Html(layout(&state.base_path, &format!("Run {}", &result.run_id[..8.min(result.run_id.len())]), "results", &body))
}
