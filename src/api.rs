use std::path::PathBuf;

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{delete, get, post, put},
    Router,
};
use nextnfs_server::export_manager::{ExportManagerHandle, ExportStatsSnapshot, QosConfig};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::web;

/// Who may change exports and QoS through the API (#122). Without a token
/// those routes refuse every request; reads and `/health` stay open.
#[derive(Clone, Default)]
pub struct AdminAuth {
    /// Bearer token the write routes require (`[server] api_token_file`).
    pub token: Option<String>,
    /// Canonical directories an API-added export must be inside
    /// (`[server] api_export_roots`); empty: the API adds no export (#125).
    pub export_roots: Vec<PathBuf>,
}

impl AdminAuth {
    /// Read the token file (its first line, trimmed) and canonicalize the
    /// export roots. An unreadable or empty file is an error: the server
    /// must not start believing it is protected when it is not.
    pub fn load(token_file: Option<&std::path::Path>, roots: &[String]) -> Result<Self, String> {
        let token = match token_file {
            Some(f) => {
                let text = std::fs::read_to_string(f)
                    .map_err(|e| format!("api token file {}: {}", f.display(), e))?;
                let t = text.lines().next().unwrap_or("").trim().to_string();
                if t.is_empty() {
                    return Err(format!("api token file {} is empty", f.display()));
                }
                Some(t)
            }
            None => None,
        };
        let export_roots = roots
            .iter()
            .map(|r| {
                std::path::Path::new(r)
                    .canonicalize()
                    .map_err(|e| format!("api_export_roots entry {}: {}", r, e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { token, export_roots })
    }

    /// Whether `path` may be exported through the API: only inside one of
    /// the configured roots, and nowhere when none is configured (#125).
    fn allows_path(&self, path: &std::path::Path) -> bool {
        match path.canonicalize() {
            Ok(p) => self.export_roots.iter().any(|r| p.starts_with(r)),
            Err(_) => false,
        }
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub export_manager: ExportManagerHandle,
    pub admin: AdminAuth,
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Constant-time comparison, so the token cannot be guessed byte by byte.
fn token_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Guard of the write routes: `Authorization: Bearer <token>`.
async fn require_admin(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let Some(token) = state.admin.token.as_deref() else {
        return json_error(
            StatusCode::FORBIDDEN,
            "export and QoS changes through the API are off: the server has no api_token_file",
        );
    };
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match given {
        Some(g) if token_eq(g.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => {
            warn!(method = %req.method(), path = %req.uri().path(), "API write refused: bad or missing token");
            let mut resp = json_error(StatusCode::UNAUTHORIZED, "a valid bearer token is required");
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
            resp
        }
    }
}

#[derive(Serialize)]
struct ExportResponse {
    name: String,
    path: String,
    read_only: bool,
    export_id: u8,
    stats: ExportStatsSnapshot,
}

#[derive(Deserialize)]
pub struct AddExportRequest {
    name: String,
    path: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    exports: usize,
}

#[derive(Serialize)]
struct StatsResponse {
    total_reads: u64,
    total_writes: u64,
    total_bytes_read: u64,
    total_bytes_written: u64,
    total_ops: u64,
    exports: Vec<ExportResponse>,
}

pub fn router(state: ApiState) -> Router {
    // Changes need the admin token (#122).
    let writes = Router::new()
        .route("/api/v1/exports", post(add_export))
        .route("/api/v1/exports/{name}", delete(remove_export))
        .route("/api/v1/qos/{name}", put(set_qos))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));
    Router::new()
        // Web UI routes
        .route("/", get(ui_dashboard))
        .route("/ui/exports", get(ui_exports))
        .route("/ui/stats", get(ui_stats))
        // REST API routes
        .route("/health", get(health))
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/stats", get(server_stats))
        .route("/api/v1/stats/{name}", get(export_stats))
        .route("/api/v1/qos/{name}", get(get_qos))
        .merge(writes)
        .with_state(state)
}

pub async fn start_api_server(bind: String, export_manager: ExportManagerHandle, admin: AdminAuth) {
    if admin.token.is_none() {
        info!("API is read-only: no api_token_file, so exports and QoS cannot be changed through it");
    }
    let state = ApiState { export_manager, admin };
    let app = router(state);

    let listener = TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        error!("failed to bind API server to {}: {}", bind, e);
        std::process::exit(1);
    });
    info!(%bind, "nextnfs REST API + Web UI listening");
    axum::serve(listener, app).await.unwrap();
}

// --- REST API handlers ---

async fn health(State(state): State<ApiState>) -> Json<HealthResponse> {
    let exports = state.export_manager.list_exports().await;
    Json(HealthResponse {
        status: "ok".to_string(),
        exports: exports.len(),
    })
}

async fn list_exports(State(state): State<ApiState>) -> Json<Vec<ExportResponse>> {
    let exports = state.export_manager.list_exports().await;
    let resp: Vec<ExportResponse> = exports
        .iter()
        .map(|e| ExportResponse {
            name: e.name.clone(),
            path: e.path.display().to_string(),
            read_only: e.read_only,
            export_id: e.export_id,
            stats: e.stats.snapshot(),
        })
        .collect();
    Json(resp)
}

async fn add_export(
    State(state): State<ApiState>,
    Json(req): Json<AddExportRequest>,
) -> impl IntoResponse {
    if state.admin.export_roots.is_empty() {
        return json_error(
            StatusCode::FORBIDDEN,
            "adding exports through the API is off: the server has no api_export_roots",
        );
    }
    if !state.admin.allows_path(std::path::Path::new(&req.path)) {
        return json_error(
            StatusCode::FORBIDDEN,
            &format!("{} is not inside the api_export_roots", req.path),
        );
    }
    match state
        .export_manager
        .add_export(req.name.clone(), PathBuf::from(&req.path), req.read_only)
        .await
    {
        Ok(info) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "name": info.name,
                "path": info.path.display().to_string(),
                "read_only": info.read_only,
                "export_id": info.export_id,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn remove_export(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.export_manager.remove_export(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn server_stats(State(state): State<ApiState>) -> Json<StatsResponse> {
    let exports = state.export_manager.list_exports().await;
    let mut total_reads = 0u64;
    let mut total_writes = 0u64;
    let mut total_bytes_read = 0u64;
    let mut total_bytes_written = 0u64;
    let mut total_ops = 0u64;

    let export_list: Vec<ExportResponse> = exports
        .iter()
        .map(|e| {
            let s = e.stats.snapshot();
            total_reads += s.reads;
            total_writes += s.writes;
            total_bytes_read += s.bytes_read;
            total_bytes_written += s.bytes_written;
            total_ops += s.ops;
            ExportResponse {
                name: e.name.clone(),
                path: e.path.display().to_string(),
                read_only: e.read_only,
                export_id: e.export_id,
                stats: s,
            }
        })
        .collect();

    Json(StatsResponse {
        total_reads,
        total_writes,
        total_bytes_read,
        total_bytes_written,
        total_ops,
        exports: export_list,
    })
}

async fn export_stats(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let exports = state.export_manager.list_exports().await;
    match exports.iter().find(|e| e.name == name) {
        Some(e) => {
            let s = e.stats.snapshot();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "name": e.name,
                    "path": e.path.display().to_string(),
                    "read_only": e.read_only,
                    "export_id": e.export_id,
                    "stats": s,
                })),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("export '{}' not found", name) })),
        )
            .into_response(),
    }
}

// --- QoS handlers ---

async fn get_qos(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.export_manager.get_qos(&name).await {
        Some(config) => (StatusCode::OK, Json(serde_json::json!(config))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("export '{}' not found", name) })),
        )
            .into_response(),
    }
}

async fn set_qos(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(config): Json<QosConfig>,
) -> impl IntoResponse {
    match state.export_manager.set_qos(&name, config).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

// --- Web UI handlers ---

async fn ui_dashboard(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_dashboard(&exports))
}

async fn ui_exports(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_exports(&exports, state.admin.token.is_some()))
}

async fn ui_stats(State(state): State<ApiState>) -> Html<String> {
    let exports = state.export_manager.list_exports().await;
    Html(web::render_stats(&exports))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("nextnfs-api-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app(admin: AdminAuth) -> (Router, ExportManagerHandle) {
        let em = ExportManagerHandle::new();
        (router(ApiState { export_manager: em.clone(), admin }), em)
    }

    fn add_req(path: &std::path::Path, auth: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/api/v1/exports")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        b.body(Body::from(
            serde_json::json!({"name": "x", "path": path.display().to_string()}).to_string(),
        ))
        .unwrap()
    }

    /// A token, and `root` as the one directory the API may export under.
    fn admin(token: &str, root: &std::path::Path) -> AdminAuth {
        AdminAuth { token: Some(token.to_string()), export_roots: vec![root.canonicalize().unwrap()] }
    }

    #[tokio::test]
    async fn writes_refused_without_configured_token() {
        let dir = Scratch::new("notoken");
        let (app, em) = app(AdminAuth::default());
        let r = app.clone().oneshot(add_req(&dir.0, Some("Bearer anything"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(Request::builder().method("DELETE").uri("/api/v1/exports/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/qos/x")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(em.list_exports().await.is_empty());
        // Reads and health stay open.
        let r = app.clone().oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app.oneshot(Request::builder().uri("/api/v1/exports").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn writes_need_the_right_token() {
        let dir = Scratch::new("token");
        let (app, em) = app(admin("s3cret", &dir.0));
        for bad in [None, Some("Bearer wrong"), Some("Bearer s3cre"), Some("s3cret")] {
            let r = app.clone().oneshot(add_req(&dir.0, bad)).await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{:?}", bad);
        }
        assert!(em.list_exports().await.is_empty());
        let r = app.clone().oneshot(add_req(&dir.0, Some("Bearer s3cret"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(em.list_exports().await.len(), 1);
        let del = |auth: &str| {
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/exports/x")
                .header(header::AUTHORIZATION, auth)
                .body(Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(del("Bearer nope")).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = app.oneshot(del("Bearer s3cret")).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(em.list_exports().await.is_empty());
    }

    /// #125: a valid token alone does not let the API export a directory;
    /// with no `api_export_roots` it exports nothing (not `/`, not the
    /// directory asked for), while remove and QoS still work with the token.
    #[tokio::test]
    async fn no_export_roots_means_no_api_exports() {
        let dir = Scratch::new("noroots");
        let a = AdminAuth { token: Some("t".to_string()), export_roots: vec![] };
        let (app, em) = app(a);
        for p in [std::path::Path::new("/"), dir.0.as_path()] {
            let r = app.clone().oneshot(add_req(p, Some("Bearer t"))).await.unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", p.display());
        }
        assert!(em.list_exports().await.is_empty());
        em.add_export("cfg".to_string(), dir.0.clone(), false).await.unwrap();
        let r = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/exports/cfg")
                    .header(header::AUTHORIZATION, "Bearer t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(em.list_exports().await.is_empty());
    }

    #[tokio::test]
    async fn export_roots_confine_added_exports() {
        let root = Scratch::new("roots");
        std::fs::create_dir(root.0.join("inside")).unwrap();
        let outside = Scratch::new("roots-out");
        let (app, em) = app(admin("t", &root.0.join("inside")));
        let r = app.clone().oneshot(add_req(&outside.0, Some("Bearer t"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // `..` out of a root is resolved before the check.
        let r = app.clone().oneshot(add_req(&root.0.join("inside/.."), Some("Bearer t"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(em.list_exports().await.is_empty());
        let r = app.oneshot(add_req(&root.0.join("inside"), Some("Bearer t"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
    }

    #[test]
    fn token_file_loads_first_line_and_rejects_empty() {
        let dir = Scratch::new("tokfile");
        let f = dir.0.join("tok");
        std::fs::write(&f, "abc\n").unwrap();
        assert_eq!(AdminAuth::load(Some(&f), &[]).unwrap().token.as_deref(), Some("abc"));
        std::fs::write(&f, "\n").unwrap();
        assert!(AdminAuth::load(Some(&f), &[]).is_err());
        assert!(AdminAuth::load(Some(&dir.0.join("missing")), &[]).is_err());
        assert!(AdminAuth::load(None, &[]).unwrap().token.is_none());
        assert!(AdminAuth::load(None, &["/nonexistent-122".to_string()]).is_err());
    }
}
