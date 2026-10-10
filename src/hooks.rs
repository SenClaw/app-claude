//! Claude Code `http` hook receiver. The worker session is started with a
//! `Stop` hook pointing here, authenticated by the run's ephemeral token.
//! When the run has a verify command, finishing is gated on it: a failing
//! check answers `{"decision":"block","reason":…}` and Claude keeps working
//! with the failure output in hand — up to `maxStopBlocks` times, after which
//! the post-run verify decides.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::registry;

/// Path the worker's hook settings point at, on this app's own loopback port.
/// The app-token guard skips `/hooks/*`; the run token is checked here instead.
pub const STOP_PATH: &str = "/hooks/stop";

/// The `--settings` hooks block for one run.
pub fn stop_hook_settings(base_url: &str, token: &str, timeout_secs: u64) -> Value {
    json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "http",
                    "url": format!("{}{}", base_url.trim_end_matches('/'), STOP_PATH),
                    "headers": { "Authorization": format!("Bearer {token}") },
                    "timeout": timeout_secs,
                }]
            }]
        }
    })
}

/// What the hook answers, given the run and the verify outcome (if one ran).
pub fn decision(entry: &registry::RunEntry, verify: Option<&super::verify::VerifyResult>) -> Value {
    match verify {
        Some(v) if !v.passed => json!({
            "decision": "block",
            "reason": format!(
                "SenClaw verification failed — fix it, then finish again (attempt {}/{}).\n{}",
                entry.stop_blocks + 1,
                entry.max_stop_blocks,
                v.report()
            ),
        }),
        _ => json!({}),
    }
}

pub async fn stop_hook(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let Some(token) = crate::procs::bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(entry) = registry::by_token(&token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Some(sid) = body["session_id"].as_str() {
        let sid = sid.to_string();
        registry::update(&entry.run_id, |e| e.session_id = Some(sid));
    }
    let Some(cmd) = entry.verify_command.clone() else {
        return Json(json!({})).into_response();
    };
    if entry.stop_blocks >= entry.max_stop_blocks {
        return Json(json!({})).into_response();
    }
    registry::update(&entry.run_id, |e| e.phase = "verifying".into());
    let v = super::verify::run(
        &cmd,
        std::path::Path::new(&entry.cwd),
        Duration::from_secs(entry.verify_timeout_secs),
        &entry.run_id,
    )
    .await;
    let answer = decision(&entry, Some(&v));
    registry::update(&entry.run_id, |e| {
        e.phase = "running".into();
        if !v.passed {
            e.stop_blocks += 1;
        }
        e.last_activity = format!("stop-hook verify {}", if v.passed { "passed" } else { "failed" });
    });
    Json(answer).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::VerifyResult;

    fn entry() -> registry::RunEntry {
        let (id, tok) = registry::register(registry::NewRun {
            item_id: "hook-1".into(),
            cwd: "/tmp".into(),
            verify_command: Some("true".into()),
            verify_timeout_secs: 5,
            max_stop_blocks: 3,
        });
        let e = registry::by_token(&tok).unwrap();
        registry::finish(&id, "done", app_space_sdk::dispatch::Outcome::TimedOut);
        e
    }

    #[test]
    fn a_failing_check_blocks_and_a_passing_one_lets_go() {
        let e = entry();
        let bad = VerifyResult { command: "t".into(), passed: false, exit_code: Some(1), timed_out: false, output_tail: "E1".into() };
        let d = decision(&e, Some(&bad));
        assert_eq!(d["decision"], "block");
        assert!(d["reason"].as_str().unwrap().contains("E1"));
        let good = VerifyResult { passed: true, ..bad };
        assert_eq!(decision(&e, Some(&good)), json!({}));
        assert_eq!(decision(&e, None), json!({}));
    }

    #[test]
    fn hook_settings_point_at_the_daemon_with_the_run_token() {
        let s = stop_hook_settings("http://127.0.0.1:18788/", "ccr_x", 930);
        let h = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(h["type"], "http");
        assert_eq!(h["url"], "http://127.0.0.1:18788/hooks/stop");
        assert_eq!(h["headers"]["Authorization"], "Bearer ccr_x");
        assert_eq!(h["timeout"], 930);
    }
}
