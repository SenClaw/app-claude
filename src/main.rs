//! app-claude — a SenClaw Space App (background) that runs kanban cards
//! assigned to `claude-code` in the user's own Claude Code CLI. The daemon
//! hands it each card (`app_space_sdk::worker`); the app runs Claude Code in
//! the card's worktree, verifies the result itself (Stop-hook gate, an
//! independent verify run, a read-only reviewer session, fix rounds) and
//! reports the outcome the daemon closes the card with.

mod api;
mod hooks;
mod procs;
mod registry;
mod runner;
mod settings;
mod verify;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;

/// `$SENCLAW_HOME/space-app-data/claude`, where Space Apps keep their own data.
fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("APP_CLAUDE_DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var("SENCLAW_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".senclaw")
    });
    home.join("space-app-data").join("claude")
}

/// Resolves on SIGTERM (the supervisor's stop) or Ctrl-C.
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("[app-claude] stopping — killing running Claude sessions");
    procs::kill_all();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init();
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(4810);
    // Loopback by default: the app authenticates only the daemon, and the
    // daemon reaches it over 127.0.0.1. SENCLAW_BIND_HOST opts in to more.
    let host = std::env::var("SENCLAW_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let data = data_dir();
    std::fs::create_dir_all(&data)?;
    let state = Arc::new(api::AppState {
        settings_path: data.join("settings.json"),
        daemon_base: std::env::var("SENCLAW_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:18788".into()),
    });
    // The Stop hook is called by the claude process on this machine.
    let hook_base = format!("http://127.0.0.1:{port}");
    let worker = worker::ClaudeCodeWorker::new(state.settings_path.clone(), hook_base, data.join("runs"));
    let app = api::router(state, Arc::new(worker::AppWorker(worker)));
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    tracing::info!("[app-claude] listening on http://{host}:{port}");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    // Claude sessions and verify commands lead their own process groups, which
    // the supervisor's signal to this app never reaches.
    procs::kill_all();
    Ok(())
}
