/// Dracula dark theme CSS matching stormd's exact color palette.
pub const CSS: &str = r#"
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
:root{
  --bg:#0f0f1a;
  --surface:#16192e;
  --surface2:#1c2038;
  --border:#2a2d45;
  --fg:#f8f8f2;
  --muted:#6272a4;
  --green:#50fa7b;
  --red:#ff5555;
  --yellow:#f1fa8c;
  --cyan:#8be9fd;
  --purple:#bd93f9;
  --pink:#ff79c6;
  --orange:#ffb86c;
  --radius:8px;
  --shadow:0 2px 8px rgba(0,0,0,.3);
}
body{
  font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Oxygen,sans-serif;
  background:var(--bg);color:var(--fg);line-height:1.6;
  min-height:100vh;
}
a{color:var(--cyan);text-decoration:none}
a:hover{text-decoration:underline}

/* Layout */
.container{max-width:1100px;margin:0 auto;padding:24px}
nav{background:var(--surface);border-bottom:1px solid var(--border);padding:0 24px;display:flex;align-items:center;gap:32px;height:56px}
nav .brand{font-weight:700;font-size:1.1rem;color:var(--purple);letter-spacing:-.5px}
nav a{color:var(--muted);font-size:.9rem;transition:color .15s}
nav a:hover,nav a.active{color:var(--fg);text-decoration:none}

/* Cards */
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:16px;margin:24px 0}
.card{
  background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);
  padding:24px;transition:border-color .15s,transform .15s;display:block;
}
.card:hover{border-color:var(--purple);transform:translateY(-2px);text-decoration:none}
.card h3{color:var(--fg);margin-bottom:8px;font-size:1.05rem}
.card p{color:var(--muted);font-size:.9rem}
.card .stat{font-size:2rem;font-weight:700;color:var(--cyan);margin-bottom:4px}

/* Status badge */
.badge{display:inline-block;padding:2px 10px;border-radius:12px;font-size:.8rem;font-weight:600}
.badge-idle{background:rgba(98,114,164,.2);color:var(--muted)}
.badge-running{background:rgba(189,147,249,.2);color:var(--purple);animation:pulse 2s infinite}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.6}}

/* Tables */
table{width:100%;border-collapse:collapse;margin:16px 0}
th,td{padding:10px 14px;text-align:left;border-bottom:1px solid var(--border)}
th{color:var(--muted);font-weight:600;font-size:.85rem;text-transform:uppercase;letter-spacing:.5px}
tr:hover{background:var(--surface2)}
.mono{font-family:'SF Mono',Monaco,Consolas,monospace;font-size:.85rem}

/* Status indicators */
.pass{color:var(--green)}.fail{color:var(--red)}.skip{color:var(--yellow)}.error{color:var(--orange)}

/* Progress bar */
.progress-bar{background:var(--surface);border:1px solid var(--border);border-radius:4px;height:24px;overflow:hidden;margin:12px 0}
.progress-fill{height:100%;background:var(--green);transition:width .3s;border-radius:3px}
.progress-fill.has-fails{background:linear-gradient(90deg,var(--green) var(--pass-pct),var(--red) var(--pass-pct))}

/* Forms */
.form-group{margin-bottom:16px}
.form-group label{display:block;color:var(--muted);font-size:.85rem;margin-bottom:6px;font-weight:600}
input,select{
  width:100%;padding:10px 14px;background:var(--bg);border:1px solid var(--border);
  border-radius:var(--radius);color:var(--fg);font-size:.95rem;
  transition:border-color .15s;
}
input:focus,select:focus{outline:none;border-color:var(--purple)}
input::placeholder{color:var(--muted)}
.form-row{display:grid;grid-template-columns:1fr 1fr;gap:16px}
.form-row-3{display:grid;grid-template-columns:1fr 1fr 1fr;gap:16px}

/* Buttons */
.btn{
  display:inline-block;padding:10px 24px;border-radius:var(--radius);font-weight:600;
  font-size:.95rem;border:none;cursor:pointer;transition:opacity .15s,transform .1s;
}
.btn:active{transform:scale(.97)}
.btn-primary{background:var(--purple);color:#fff}
.btn-primary:hover{opacity:.9;text-decoration:none}
.btn-danger{background:var(--red);color:#fff;padding:6px 14px;font-size:.85rem}
.btn-danger:hover{opacity:.9;text-decoration:none}
.btn-sm{padding:6px 14px;font-size:.85rem}

/* Summary stats row */
.stats{display:flex;gap:24px;margin:16px 0;flex-wrap:wrap}
.stat-box{text-align:center;min-width:90px}
.stat-box .num{font-size:1.8rem;font-weight:700;line-height:1}
.stat-box .label{font-size:.8rem;color:var(--muted);margin-top:4px}

/* Filters */
.filters{display:flex;gap:8px;margin:16px 0;flex-wrap:wrap}
.filter-btn{
  padding:6px 16px;border-radius:16px;font-size:.85rem;cursor:pointer;
  background:var(--surface);border:1px solid var(--border);color:var(--muted);
  transition:all .15s;
}
.filter-btn:hover,.filter-btn.active{background:var(--purple);color:#fff;border-color:var(--purple)}

/* Misc */
.mt-16{margin-top:16px}.mt-24{margin-top:24px}.mb-16{margin-bottom:16px}
.text-muted{color:var(--muted)}.text-sm{font-size:.85rem}
h1{font-size:1.5rem;font-weight:700;margin-bottom:8px}
h2{font-size:1.2rem;font-weight:600;margin-bottom:8px;color:var(--fg)}
.empty{text-align:center;padding:48px 24px;color:var(--muted)}
.back-link{display:inline-block;margin-bottom:16px;color:var(--muted);font-size:.9rem}
.back-link:hover{color:var(--fg)}
.header-row{display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:12px}
"#;
