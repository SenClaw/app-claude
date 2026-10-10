//! HTTP surface of app-claude:
//!
//! - `/health` — supervisor probe (open).
//! - `/api/worker/runs*` — the app-worker contract the daemon drives
//!   (`app_space_sdk::worker::worker_router`).
//! - `/api/status`, `/api/runs`, `/api/settings` — the app's own UI.
//! - `/hooks/stop` — the Claude Code Stop hook (run token, checked in `hooks`).
//! - `/` — the UI page.
//!
//! Everything but `/health` and `/hooks/*` requires the app's access token, so
//! only the daemon (and its proxy, which stamps the token) reaches it.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::settings::{self, ClaudeCodeSettings};

pub struct AppState {
    pub settings_path: PathBuf,
    /// The daemon's base URL (`SENCLAW_BASE_URL`), for the `claude mcp add` hints.
    pub daemon_base: String,
}

pub fn router(state: Arc<AppState>, worker: Arc<dyn app_space_sdk::worker::Worker>) -> Router {
    let own = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/runs", get(runs))
        .route("/api/settings", get(settings_get).put(settings_put))
        .with_state(state)
        .layer(axum::middleware::from_fn(app_space_sdk::auth::require_app_token_with(&["/health", "/hooks/*"])));
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route(crate::hooks::STOP_PATH, post(crate::hooks::stop_hook))
        .nest("/api/worker", app_space_sdk::worker::worker_router(worker))
        .merge(own)
}

/// The `claude mcp add` commands for the daemon at `base`. The token is
/// referenced as an env var, never printed.
pub fn mcp_add_commands(base: &str) -> Vec<String> {
    ["core", "kanban"]
        .iter()
        .map(|s| {
            format!(
                "claude mcp add --transport http --scope user senclaw-{s} {base}/mcp/{s} --header \"Authorization: Bearer ${{SENCLAW_TOKEN}}\""
            )
        })
        .collect()
}

async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], Html(include_str!("../web/index.html"))).into_response()
}

async fn status(State(st): State<Arc<AppState>>) -> Response {
    let s = settings::load(&st.settings_path);
    let version = crate::procs::binary_version(&s.binary).await;
    Json(json!({
        "binary": s.binary,
        "installed": version.is_some(),
        "version": version,
        "assignee": "claude-code",
        "mcp": { "addCommands": mcp_add_commands(&st.daemon_base) },
    }))
    .into_response()
}

async fn runs() -> Response {
    Json(json!({ "runs": crate::registry::list() })).into_response()
}

async fn settings_get(State(st): State<Arc<AppState>>) -> Response {
    Json(settings::load(&st.settings_path)).into_response()
}

async fn settings_put(State(st): State<Arc<AppState>>, Json(body): Json<ClaudeCodeSettings>) -> Response {
    let s = match body.validated() {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };
    match settings::save(&st.settings_path, &s) {
        Ok(()) => Json(s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_commands_reference_the_token_by_name_only() {
        let c = mcp_add_commands("http://127.0.0.1:18788");
        assert_eq!(c.len(), 2);
        assert!(c[0].contains("http://127.0.0.1:18788/mcp/core"));
        assert!(c[0].contains("${SENCLAW_TOKEN}"));
    }
}
