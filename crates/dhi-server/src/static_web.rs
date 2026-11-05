//! Same-origin hosting for the exported web UI.
//!
//! The Next.js export is a directory of static files (`web/out`). The export
//! contains only the `preview` placeholder for the dynamic task routes, so the
//! static router maps every real task deep link onto those shells. The task id
//! is never interpolated into a filesystem path: only fixed preview files are
//! served.

use std::io::ErrorKind;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use tower_http::services::{ServeDir, ServeFile};
use tracing::warn;

/// Default export directory, relative to the server's working directory.
pub(crate) const DEFAULT_WEB_DIR: &str = "web/out";

const HTML: &str = "text/html; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

/// Static file router rooted at one exported web directory.
pub(crate) struct StaticWeb {
    root: PathBuf,
}

impl StaticWeb {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Builds the static router. API paths that reach this router are unknown
    /// (the real API routes live on the server router) and answer JSON, never
    /// the SPA shell.
    pub(crate) fn into_router(self) -> Router {
        let root = self.root;
        let fallback = ServeDir::new(&root)
            .append_index_html_on_directories(true)
            .fallback(ServeFile::new(root.join("index.html")));

        Router::new()
            .route("/api", get(api_not_found))
            .route("/api/", get(api_not_found))
            .route("/api/{*rest}", get(api_not_found))
            .route("/tasks", get(tasks_index))
            .route("/tasks/", get(tasks_index))
            // One catch-all because matchit does not support a parameter with
            // a dynamic suffix (`/tasks/{id}.txt`) and a wildcard cannot be
            // combined with sibling parameter routes. The handler dispatches
            // only to fixed preview files.
            .route("/tasks/{*path}", get(task_path))
            .with_state(root)
            .fallback_service(fallback)
    }
}

/// Resolves the export directory: `DHI_WEB_DIR` wins when set, otherwise the
/// default `web/out` only when it exists.
pub(crate) fn resolve_web_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DHI_WEB_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let default = PathBuf::from(DEFAULT_WEB_DIR);
    default.is_dir().then_some(default)
}

async fn api_not_found() -> Response {
    not_found()
}

async fn tasks_index(State(root): State<PathBuf>) -> Response {
    serve_file(root.join("tasks/index.html"), HTML).await
}

async fn task_path(State(root): State<PathBuf>, Path(path): Path<String>) -> Response {
    match task_asset(&path) {
        Some((relative, content_type)) => serve_file(root.join(relative), content_type).await,
        None => not_found(),
    }
}

/// Maps a task path (the wildcard remainder, without a leading slash) onto one
/// of the fixed preview files. Returns `None` for unknown shapes; nothing from
/// the path is used in the returned filename.
fn task_asset(path: &str) -> Option<(&'static str, &'static str)> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let (id, rest) = match path.split_once('/') {
        Some((id, rest)) => (id, rest),
        None => (path, ""),
    };
    if id.is_empty() {
        return None;
    }

    match rest {
        "" => match id.strip_suffix(".txt") {
            Some(stem) if !stem.is_empty() => Some(("tasks/preview/index.txt", TEXT)),
            Some(_) => None,
            None => Some(("tasks/preview/index.html", HTML)),
        },
        "index.txt" => Some(("tasks/preview/index.txt", TEXT)),
        "index.html" => Some(("tasks/preview/index.html", HTML)),
        "work" => Some(("tasks/preview/work/index.html", HTML)),
        "work.txt" => Some(("tasks/preview/work/index.txt", TEXT)),
        "work/index.txt" => Some(("tasks/preview/work/index.txt", TEXT)),
        "work/index.html" => Some(("tasks/preview/work/index.html", HTML)),
        "graph" => Some(("tasks/preview/graph/index.html", HTML)),
        "graph.txt" => Some(("tasks/preview/graph/index.txt", TEXT)),
        "graph/index.txt" => Some(("tasks/preview/graph/index.txt", TEXT)),
        "graph/index.html" => Some(("tasks/preview/graph/index.html", HTML)),
        "__next.tasks.$d$id.__PAGE__.txt" => {
            Some(("tasks/preview/__next.tasks.$d$id.__PAGE__.txt", TEXT))
        }
        "work/__next.tasks.$d$id.work.__PAGE__.txt" => Some((
            "tasks/preview/work/__next.tasks.$d$id.work.__PAGE__.txt",
            TEXT,
        )),
        "graph/__next.tasks.$d$id.graph.__PAGE__.txt" => Some((
            "tasks/preview/graph/__next.tasks.$d$id.graph.__PAGE__.txt",
            TEXT,
        )),
        _ => None,
    }
}

async fn serve_file(path: PathBuf, content_type: &'static str) -> Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
        Err(err) => {
            if err.kind() != ErrorKind::NotFound {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "static web file read failed"
                );
            }
            not_found()
        }
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "error": "not found" })),
    )
        .into_response()
}
