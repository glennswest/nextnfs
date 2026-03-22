use nextnfs_server::export_manager::ExportInfo;

const STYLE: &str = r#"
<style>
    :root {
        --bg: #0d0f1a; --surface: #16192e; --border: #2a2d45;
        --text: #e2e8f0; --dim: #8892b0; --green: #50fa7b;
        --red: #e94560; --cyan: #8be9fd; --yellow: #f1fa8c;
    }
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body {
        font-family: -apple-system, 'Segoe UI', system-ui, sans-serif;
        background: var(--bg); color: var(--text);
        line-height: 1.6; padding: 20px;
    }
    .nav { display: flex; gap: 12px; margin-bottom: 24px; }
    .nav a {
        color: var(--cyan); text-decoration: none; padding: 8px 16px;
        border: 1px solid var(--border); border-radius: 6px;
        transition: all 0.2s;
    }
    .nav a:hover, .nav a.active {
        background: var(--surface); border-color: var(--cyan);
    }
    h1 { font-size: 1.4em; margin-bottom: 8px; color: var(--cyan); }
    h2 { font-size: 1.1em; margin: 16px 0 8px; color: var(--dim); }
    .card {
        background: var(--surface); border: 1px solid var(--border);
        border-radius: 8px; padding: 16px; margin-bottom: 12px;
    }
    .stat-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 12px; }
    .stat-box {
        background: var(--bg); border: 1px solid var(--border);
        border-radius: 6px; padding: 12px; text-align: center;
    }
    .stat-box .value { font-size: 1.6em; font-weight: 700; color: var(--green); }
    .stat-box .label { font-size: 0.8em; color: var(--dim); margin-top: 4px; }
    table { width: 100%; border-collapse: collapse; }
    th, td { text-align: left; padding: 10px 14px; border-bottom: 1px solid var(--border); }
    th { color: var(--dim); font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.5px; }
    .badge {
        display: inline-block; padding: 2px 8px; border-radius: 4px;
        font-size: 0.8em; font-weight: 600;
    }
    .badge-rw { background: rgba(80,250,123,0.15); color: var(--green); }
    .badge-ro { background: rgba(241,250,140,0.15); color: var(--yellow); }
    .form-row { display: flex; gap: 8px; margin-bottom: 12px; align-items: center; }
    input[type="text"] {
        background: var(--bg); border: 1px solid var(--border); color: var(--text);
        padding: 8px 12px; border-radius: 6px; font-size: 0.9em;
    }
    input[type="text"]:focus { outline: none; border-color: var(--cyan); }
    label { color: var(--dim); font-size: 0.9em; }
    input[type="checkbox"] { accent-color: var(--cyan); }
    button, .btn {
        background: var(--cyan); color: var(--bg); border: none;
        padding: 8px 16px; border-radius: 6px; cursor: pointer;
        font-weight: 600; font-size: 0.9em; transition: opacity 0.2s;
    }
    button:hover, .btn:hover { opacity: 0.85; }
    .btn-danger { background: var(--red); color: white; }
    .btn-sm { padding: 4px 10px; font-size: 0.8em; }
    .mono { font-family: 'SF Mono', 'Fira Code', monospace; font-size: 0.9em; }
    .empty { color: var(--dim); text-align: center; padding: 32px; }
    #status { margin-top: 8px; font-size: 0.85em; }
    .status-ok { color: var(--green); }
    .status-err { color: var(--red); }
</style>
"#;

fn nav(active: &str) -> String {
    let pages = [("/", "Dashboard"), ("/ui/exports", "Exports"), ("/ui/stats", "Stats")];
    let mut html = String::from(r#"<div class="nav">"#);
    for (href, label) in &pages {
        let cls = if *label == active { " active" } else { "" };
        html.push_str(&format!(r#"<a href="{}" class="{}">{}</a>"#, href, cls, label));
    }
    html.push_str("</div>");
    html
}

fn head(title: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title>{}</head><body>"#,
        title, STYLE
    )
}

fn foot() -> &'static str {
    "</body></html>"
}

pub fn render_dashboard(exports: &[ExportInfo]) -> String {
    let mut html = head("NextNFS");
    html.push_str(&nav("Dashboard"));
    html.push_str(r#"<h1>NextNFS</h1>"#);

    // Stats overview
    html.push_str(r#"<div class="stat-grid">"#);
    html.push_str(&format!(
        r#"<div class="stat-box"><div class="value">{}</div><div class="label">Exports</div></div>"#,
        exports.len()
    ));

    let mut total_ops = 0u64;
    let mut total_reads = 0u64;
    let mut total_writes = 0u64;
    for e in exports {
        let s = e.stats.snapshot();
        total_ops += s.ops;
        total_reads += s.reads;
        total_writes += s.writes;
    }
    html.push_str(&format!(
        r#"<div class="stat-box"><div class="value">{}</div><div class="label">Total Ops</div></div>"#,
        total_ops
    ));
    html.push_str(&format!(
        r#"<div class="stat-box"><div class="value">{}</div><div class="label">Reads</div></div>"#,
        total_reads
    ));
    html.push_str(&format!(
        r#"<div class="stat-box"><div class="value">{}</div><div class="label">Writes</div></div>"#,
        total_writes
    ));
    html.push_str("</div>");

    // Export list
    html.push_str(r#"<h2>Exports</h2><div class="card">"#);
    if exports.is_empty() {
        html.push_str(r#"<div class="empty">No exports configured</div>"#);
    } else {
        html.push_str("<table><tr><th>Name</th><th>Path</th><th>Mode</th><th>ID</th></tr>");
        for e in exports {
            let badge = if e.read_only {
                r#"<span class="badge badge-ro">RO</span>"#
            } else {
                r#"<span class="badge badge-rw">RW</span>"#
            };
            html.push_str(&format!(
                r#"<tr><td>{}</td><td class="mono">{}</td><td>{}</td><td>{}</td></tr>"#,
                e.name,
                e.path.display(),
                badge,
                e.export_id
            ));
        }
        html.push_str("</table>");
    }
    html.push_str("</div>");

    // Auto-refresh script
    html.push_str(r#"<script>setTimeout(()=>location.reload(), 5000);</script>"#);
    html.push_str(foot());
    html
}

pub fn render_exports(exports: &[ExportInfo]) -> String {
    let mut html = head("NextNFS - Exports");
    html.push_str(&nav("Exports"));
    html.push_str(r#"<h1>Export Management</h1>"#);

    // Add export form
    html.push_str(r#"<div class="card"><h2>Add Export</h2>"#);
    html.push_str(r#"<div class="form-row"><input type="text" id="exp-name" placeholder="Export name" style="width:160px">"#);
    html.push_str(r#"<input type="text" id="exp-path" placeholder="/path/to/share" style="width:280px">"#);
    html.push_str(r#"<label><input type="checkbox" id="exp-ro"> Read-only</label>"#);
    html.push_str(r#"<button onclick="addExport()">Add</button></div>"#);
    html.push_str(r#"<div id="status"></div></div>"#);

    // Export list
    html.push_str(r#"<div class="card"><h2>Current Exports</h2>"#);
    if exports.is_empty() {
        html.push_str(r#"<div class="empty">No exports configured</div>"#);
    } else {
        html.push_str("<table><tr><th>Name</th><th>Path</th><th>Mode</th><th>ID</th><th></th></tr>");
        for e in exports {
            let badge = if e.read_only {
                r#"<span class="badge badge-ro">RO</span>"#
            } else {
                r#"<span class="badge badge-rw">RW</span>"#
            };
            html.push_str(&format!(
                r#"<tr><td>{name}</td><td class="mono">{path}</td><td>{badge}</td><td>{id}</td><td><button class="btn-danger btn-sm" onclick="removeExport('{name}')">Remove</button></td></tr>"#,
                name = e.name,
                path = e.path.display(),
                badge = badge,
                id = e.export_id
            ));
        }
        html.push_str("</table>");
    }
    html.push_str("</div>");

    html.push_str(r#"<script>
async function addExport() {
    const name = document.getElementById('exp-name').value;
    const path = document.getElementById('exp-path').value;
    const ro = document.getElementById('exp-ro').checked;
    const st = document.getElementById('status');
    if (!name || !path) { st.innerHTML='<span class="status-err">Name and path required</span>'; return; }
    try {
        const r = await fetch('/api/v1/exports', {
            method: 'POST', headers: {'Content-Type':'application/json'},
            body: JSON.stringify({name, path, read_only: ro})
        });
        const d = await r.json();
        if (r.ok) { st.innerHTML='<span class="status-ok">Export added</span>'; setTimeout(()=>location.reload(),500); }
        else { st.innerHTML='<span class="status-err">'+d.error+'</span>'; }
    } catch(e) { st.innerHTML='<span class="status-err">'+e+'</span>'; }
}
async function removeExport(name) {
    if (!confirm('Remove export "'+name+'"?')) return;
    try {
        const r = await fetch('/api/v1/exports/'+name, {method:'DELETE'});
        if (r.ok) location.reload();
        else { const d = await r.json(); alert(d.error); }
    } catch(e) { alert(e); }
}
</script>"#);
    html.push_str(foot());
    html
}

pub fn render_stats(exports: &[ExportInfo]) -> String {
    let mut html = head("NextNFS - Stats");
    html.push_str(&nav("Stats"));
    html.push_str(r#"<h1>Statistics</h1>"#);

    if exports.is_empty() {
        html.push_str(r#"<div class="card"><div class="empty">No exports configured</div></div>"#);
    } else {
        for e in exports {
            let s = e.stats.snapshot();
            html.push_str(&format!(r#"<div class="card"><h2>{}</h2>"#, e.name));
            html.push_str(&format!(
                r#"<div style="font-size:0.85em;color:var(--dim);margin-bottom:8px" class="mono">{}</div>"#,
                e.path.display()
            ));
            html.push_str(r#"<div class="stat-grid">"#);
            html.push_str(&stat_box(&format_count(s.reads), "Reads"));
            html.push_str(&stat_box(&format_count(s.writes), "Writes"));
            html.push_str(&stat_box(&format_bytes(s.bytes_read), "Bytes Read"));
            html.push_str(&stat_box(&format_bytes(s.bytes_written), "Bytes Written"));
            html.push_str(&stat_box(&format_count(s.ops), "Ops"));
            html.push_str("</div></div>");
        }
    }

    html.push_str(r#"<script>setTimeout(()=>location.reload(), 3000);</script>"#);
    html.push_str(foot());
    html
}

fn stat_box(value: &str, label: &str) -> String {
    format!(
        r#"<div class="stat-box"><div class="value">{}</div><div class="label">{}</div></div>"#,
        value, label
    )
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_bytes(n: u64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.1} GB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}
