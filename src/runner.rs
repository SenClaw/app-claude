//! Spawns the user's own `claude` binary headless (`-p`, stream-json out) and
//! folds its event stream into a [`RunResult`]. SenClaw never touches Claude
//! credentials: the child authenticates exactly as the user's CLI does.
//!
//! Event shapes (observed from Claude Code 2.1.x `--output-format stream-json
//! --verbose`): `{"type":"system","subtype":"init","session_id":…}`,
//! `{"type":"assistant","message":{"content":[{"type":"text"|"tool_use",…}]}}`,
//! `{"type":"result","is_error":…,"result":…,"session_id":…,
//! "total_cost_usd":…,"num_turns":…,"structured_output":…}`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// SenClaw credentials kept out of the child's environment. Defense in depth
/// only — the worker is NOT isolated from SenClaw: it runs as the user, and
/// with Bash allowed it can read `~/.senclaw/api_token` like any local process.
const SCRUBBED_ENV: &[&str] = &["SENCLAW_API_TOKEN", "SENCLAW_WS_TOKEN", "SENCLAW_TOKEN_ACCESS_APP", "SENCLAW_APP_TOKEN"];

/// How much of stderr to keep for error reports.
const STDERR_TAIL: usize = 4000;

#[derive(Debug, Clone)]
pub enum Session {
    /// Start a new session with this id.
    New(String),
    /// Continue an existing session.
    Resume(String),
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub binary: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub session: Session,
    pub append_system_prompt: Option<String>,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_budget_usd: Option<f64>,
    pub json_schema: Option<Value>,
    /// `{"mcpServers": {…}}`. Always paired with `--strict-mcp-config`, so the
    /// session sees exactly these servers and none of the user's own.
    pub mcp_config: Value,
    /// Extra settings (hooks). Written to a 0600 file, never put on argv.
    pub settings: Option<Value>,
    /// Per-run scratch dir for the config files above.
    pub run_dir: PathBuf,
    pub timeout: Duration,
}

/// One parsed stream-json line.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Init { session_id: String, model: Option<String> },
    Text(String),
    ToolUse { name: String },
    Result(ResultEvent),
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultEvent {
    pub is_error: bool,
    pub subtype: String,
    pub result: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub num_turns: u64,
    pub structured: Option<Value>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub session_id: Option<String>,
    pub result: Option<ResultEvent>,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
}

impl RunResult {
    /// A run that produced a non-error result event.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && matches!(&self.result, Some(r) if !r.is_error)
    }

    /// Human-readable failure reason.
    pub fn error_text(&self) -> String {
        if self.timed_out {
            return "claude-code: run timed out".into();
        }
        match &self.result {
            Some(r) if r.is_error => format!("claude-code: {}", first_line(&r.result)),
            Some(_) => String::new(),
            None => format!(
                "claude-code exited (code {:?}) without a result: {}",
                self.exit_code,
                first_line(&self.stderr_tail)
            ),
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(300).collect()
}

/// Parse one stream-json line. Unknown or malformed lines are [`StreamEvent::Other`].
pub fn parse_line(line: &str) -> StreamEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return StreamEvent::Other;
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => match v["session_id"].as_str() {
            Some(sid) => StreamEvent::Init {
                session_id: sid.to_string(),
                model: v["model"].as_str().map(str::to_string),
            },
            None => StreamEvent::Other,
        },
        Some("assistant") => {
            // One event per message; surface the first tool call or the text.
            let content = v["message"]["content"].as_array().cloned().unwrap_or_default();
            if let Some(tu) = content.iter().find(|c| c["type"] == "tool_use") {
                return StreamEvent::ToolUse {
                    name: tu["name"].as_str().unwrap_or("").to_string(),
                };
            }
            let text: Vec<&str> = content
                .iter()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect();
            if text.is_empty() {
                StreamEvent::Other
            } else {
                StreamEvent::Text(text.join("\n"))
            }
        }
        Some("result") => StreamEvent::Result(ResultEvent {
            is_error: v["is_error"].as_bool().unwrap_or(false),
            subtype: v["subtype"].as_str().unwrap_or("").to_string(),
            result: v["result"].as_str().unwrap_or("").to_string(),
            session_id: v["session_id"].as_str().unwrap_or("").to_string(),
            cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
            num_turns: v["num_turns"].as_u64().unwrap_or(0),
            structured: v.get("structured_output").filter(|s| !s.is_null()).cloned(),
            terminal_reason: v["terminal_reason"].as_str().map(str::to_string),
        }),
        _ => StreamEvent::Other,
    }
}

/// The argv after the binary. Config files are referenced by path.
pub fn build_args(spec: &RunSpec, mcp_path: &Path, settings_path: Option<&Path>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--permission-mode".into(),
        spec.permission_mode.clone(),
        "--max-turns".into(),
        spec.max_turns.to_string(),
        "--mcp-config".into(),
        mcp_path.to_string_lossy().into_owned(),
        "--strict-mcp-config".into(),
    ];
    match &spec.session {
        Session::New(id) => a.extend(["--session-id".into(), id.clone()]),
        Session::Resume(id) => a.extend(["--resume".into(), id.clone()]),
    }
    if let Some(p) = settings_path {
        a.extend(["--settings".into(), p.to_string_lossy().into_owned()]);
    }
    if let Some(sp) = spec.append_system_prompt.as_deref().filter(|s| !s.is_empty()) {
        a.extend(["--append-system-prompt".into(), sp.to_string()]);
    }
    if !spec.allowed_tools.is_empty() {
        a.push("--allowedTools".into());
        a.push(spec.allowed_tools.join(","));
    }
    if !spec.disallowed_tools.is_empty() {
        a.push("--disallowedTools".into());
        a.push(spec.disallowed_tools.join(","));
    }
    if let Some(m) = spec.model.as_deref().filter(|m| !m.is_empty()) {
        a.extend(["--model".into(), m.to_string()]);
    }
    if let Some(b) = spec.max_budget_usd {
        a.extend(["--max-budget-usd".into(), format!("{b}")]);
    }
    if let Some(s) = &spec.json_schema {
        a.extend(["--json-schema".into(), s.to_string()]);
    }
    a
}

fn write_private(path: &Path, v: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(v)?).with_context(|| format!("write {}", path.display()))?;
    crate::procs::restrict(path);
    Ok(())
}

/// Run one session to completion. `on_event` sees every parsed event as it
/// arrives (progress reporting). The config files are removed afterwards —
/// they can carry the run token and MCP env.
pub async fn run(spec: &RunSpec, mut on_event: impl FnMut(&StreamEvent)) -> Result<RunResult> {
    std::fs::create_dir_all(&spec.run_dir).with_context(|| format!("create {}", spec.run_dir.display()))?;
    let tag = match &spec.session {
        Session::New(id) | Session::Resume(id) => id.clone(),
    };
    let mcp_path = spec.run_dir.join(format!("mcp-{tag}-{}.json", uuid::Uuid::new_v4()));
    write_private(&mcp_path, &spec.mcp_config)?;
    let settings_path = match &spec.settings {
        Some(s) => {
            let p = spec.run_dir.join(format!("settings-{tag}-{}.json", uuid::Uuid::new_v4()));
            write_private(&p, s)?;
            Some(p)
        }
        None => None,
    };
    let result = run_child(spec, &mcp_path, settings_path.as_deref(), &mut on_event).await;
    let _ = std::fs::remove_file(&mcp_path);
    if let Some(p) = &settings_path {
        let _ = std::fs::remove_file(p);
    }
    result
}

async fn run_child(
    spec: &RunSpec,
    mcp_path: &Path,
    settings_path: Option<&Path>,
    on_event: &mut impl FnMut(&StreamEvent),
) -> Result<RunResult> {
    let mut cmd = tokio::process::Command::new(&spec.binary);
    cmd.args(build_args(spec, mcp_path, settings_path))
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn `{}` (is Claude Code installed?)", spec.binary))?;

    // The prompt goes over stdin, not argv: no length limit, not visible in `ps`.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(spec.prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let pgid = child.id();
    let tag = spec.run_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let _group = crate::procs::track(pgid, &tag);
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    let stderr_task = tokio::spawn(crate::procs::read_tail(stderr, STDERR_TAIL));

    let mut out = RunResult::default();
    let mut lines = BufReader::new(stdout).lines();
    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let ev = parse_line(&line);
            match &ev {
                StreamEvent::Init { session_id, .. } => out.session_id = Some(session_id.clone()),
                StreamEvent::Result(r) => {
                    if !r.session_id.is_empty() {
                        out.session_id = Some(r.session_id.clone());
                    }
                    out.result = Some(r.clone());
                }
                _ => {}
            }
            on_event(&ev);
        }
    };
    if tokio::time::timeout(spec.timeout, read).await.is_err() {
        out.timed_out = true;
        crate::procs::kill_group(pgid);
    }
    let status = child.wait().await.ok();
    out.exit_code = status.and_then(|s| s.code());
    // Whatever the session left running in its group (background Bash jobs,
    // dev servers) ends with it — and can no longer hold stderr open.
    crate::procs::kill_group(pgid);
    out.stderr_tail = tokio::time::timeout(Duration::from_secs(5), stderr_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_observed_event_shapes() {
        let init = r#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-haiku","tools":[]}"#;
        assert_eq!(
            parse_line(init),
            StreamEvent::Init { session_id: "s-1".into(), model: Some("claude-haiku".into()) }
        );
        let hook = r#"{"type":"system","subtype":"hook_started","session_id":"s-1"}"#;
        assert_eq!(parse_line(hook), StreamEvent::Other);
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(parse_line(text), StreamEvent::Text("hi".into()));
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"},{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        assert_eq!(parse_line(tool), StreamEvent::ToolUse { name: "Bash".into() });
        let res = r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate","session_id":"s-1","total_cost_usd":0,"num_turns":1,"terminal_reason":"api_error"}"#;
        match parse_line(res) {
            StreamEvent::Result(r) => {
                assert!(r.is_error);
                assert_eq!(r.terminal_reason.as_deref(), Some("api_error"));
                assert!(r.structured.is_none());
            }
            other => panic!("{other:?}"),
        }
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"","session_id":"s-2","total_cost_usd":0.12,"num_turns":4,"structured_output":{"status":"done"}}"#;
        match parse_line(ok) {
            StreamEvent::Result(r) => {
                assert_eq!(r.structured.unwrap()["status"], "done");
                assert_eq!(r.num_turns, 4);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_line("not json"), StreamEvent::Other);
    }

    fn spec(session: Session) -> RunSpec {
        RunSpec {
            binary: "claude".into(),
            cwd: "/tmp".into(),
            prompt: "p".into(),
            session,
            append_system_prompt: Some("guide".into()),
            permission_mode: "acceptEdits".into(),
            allowed_tools: vec!["Bash".into(), "mcp__senclaw-kanban".into()],
            disallowed_tools: vec![],
            model: None,
            max_turns: 9,
            max_budget_usd: None,
            json_schema: Some(serde_json::json!({"type": "object"})),
            mcp_config: serde_json::json!({"mcpServers": {}}),
            settings: None,
            run_dir: "/tmp".into(),
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn args_are_headless_strict_and_never_carry_the_prompt() {
        let a = build_args(&spec(Session::New("u-1".into())), Path::new("/r/mcp.json"), Some(Path::new("/r/s.json")));
        let s = a.join(" ");
        assert!(s.starts_with("-p --output-format stream-json --verbose"));
        assert!(s.contains("--strict-mcp-config"));
        assert!(s.contains("--session-id u-1"));
        assert!(s.contains("--settings /r/s.json"));
        assert!(s.contains("--allowedTools Bash,mcp__senclaw-kanban"));
        assert!(s.contains("--max-turns 9"));
        assert!(!a.iter().any(|x| x == "p"));
        let r = build_args(&spec(Session::Resume("u-1".into())), Path::new("/r/m.json"), None);
        assert!(r.join(" ").contains("--resume u-1"));
        assert!(!r.iter().any(|x| x == "--settings"));
    }

    #[test]
    fn failure_text_explains_what_happened() {
        let mut r = RunResult::default();
        r.exit_code = Some(1);
        r.stderr_tail = "\nboom\n".into();
        assert!(r.error_text().contains("boom"));
        r.timed_out = true;
        assert!(r.error_text().contains("timed out"));
        assert!(!r.succeeded());
    }
}
