//! Runs a dispatched work item in Claude Code and decides its outcome from
//! evidence SenClaw gathers itself:
//!
//! 1. the worker session (Stop hook gates finishing on the verify command),
//! 2. an independent re-run of the verify command after the session ends,
//! 3. a read-only reviewer session over the worktree diff,
//! 4. on failure, feedback goes back into the same session (`--resume`) up to
//!    `maxFixRounds` times; then the item is blocked for a person.
//!
//! The model's own report only ever *lowers* the outcome (it may say
//! "blocked"); it can never mark work done on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use app_space_sdk::dispatch::Outcome;
use app_space_sdk::worker::{Job, JobMcp, RunStatus, Worker};
use serde_json::{json, Value};

use crate::registry;
use crate::runner::{self, RunSpec, Session, StreamEvent};
use crate::settings::{self, ClaudeCodeSettings};
use crate::verify::{self, VerifyResult};

/// Largest diff handed to the reviewer.
const REVIEW_DIFF_BYTES: usize = 60 * 1024;

const WORKER_GUIDANCE: &str = "\
## SenClaw worker rules
- You are running a task dispatched by SenClaw. Work only inside the current directory.
- Post short progress notes with `kanban_comment` when the kanban tools are available.
- Do NOT close or block the card yourself: SenClaw verifies your work and closes it.
- Run the project's tests for what you changed. When a verification failure is reported back to \
you, fix the cause; never weaken or delete tests to make them pass.
- Commit your changes with git when you are done (clear conventional commit message).
- Your final answer is the structured result: status `done` when finished, `blocked` when you \
need a person (say exactly what you need in `summary`).";

fn result_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["done", "blocked"] },
            "summary": { "type": "string" },
            "files_changed": { "type": "array", "items": { "type": "string" } },
            "tests_run": { "type": "array", "items": { "type": "string" } },
            "risks": { "type": "string" }
        },
        "required": ["status", "summary"]
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "verdict": { "type": "string", "enum": ["pass", "changes_requested"] },
            "summary": { "type": "string" },
            "issues": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["verdict", "summary"]
    })
}

/// The worker's structured report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub blocked: bool,
    pub summary: String,
}

/// Structured output when present; else a JSON object at the end of the text;
/// else the first line of the text, read as "done".
pub fn report_from(structured: Option<&Value>, text: &str) -> Report {
    let parsed = structured.cloned().or_else(|| {
        let start = text.rfind("{\"status\"").or_else(|| text.rfind('{'))?;
        serde_json::from_str::<Value>(text[start..].trim().trim_end_matches("```").trim()).ok()
    });
    match parsed {
        Some(v) if v.get("status").is_some() => Report {
            blocked: v["status"] == "blocked",
            summary: v["summary"].as_str().unwrap_or("").trim().to_string(),
        },
        _ => Report {
            blocked: false,
            summary: text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub pass: bool,
    pub summary: String,
    pub issues: Vec<String>,
}

pub fn review_from(structured: Option<&Value>, text: &str) -> Review {
    let v = structured.cloned().or_else(|| {
        let start = text.rfind('{')?;
        serde_json::from_str::<Value>(&text[start..]).ok()
    });
    match v {
        Some(v) => Review {
            pass: v["verdict"] == "pass",
            summary: v["summary"].as_str().unwrap_or("").to_string(),
            issues: v["issues"]
                .as_array()
                .map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        },
        // An unreadable verdict is not a pass.
        None => Review { pass: false, summary: "reviewer gave no verdict".into(), issues: vec![] },
    }
}

/// Feedback prompt for the next fix round.
pub fn feedback(verify: Option<&VerifyResult>, review: Option<&Review>) -> String {
    let mut parts = vec!["SenClaw checked your work and it is not done yet.".to_string()];
    if let Some(v) = verify.filter(|v| !v.passed) {
        parts.push(format!("Verification failed:\n{}", v.report()));
    }
    if let Some(r) = review.filter(|r| !r.pass) {
        let mut s = format!("Code review requested changes: {}", r.summary);
        for i in &r.issues {
            s.push_str(&format!("\n- {i}"));
        }
        parts.push(s);
    }
    parts.push("Fix these, commit, and give the structured result again.".into());
    parts.join("\n\n")
}

/// Claude Code MCP config for the HTTP servers the daemon handed over (its
/// `/mcp/*` bridge with a job-scoped token; the daemon pins the chat SenClaw's
/// tools act for in that token).
pub fn mcp_config(servers: &[JobMcp]) -> Value {
    let mut map = serde_json::Map::new();
    for s in servers {
        map.insert(s.name.clone(), json!({ "type": "http", "url": s.url, "headers": s.headers }));
    }
    json!({ "mcpServers": map })
}

/// The change a run made, for the reviewer: `git diff <base>` in the worktree
/// (committed and uncommitted work), plus untracked files.
pub struct Diff {
    pub stat: String,
    pub diff: String,
    pub untracked: Vec<String>,
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn diff_against(cwd: &Path, base: &str) -> Option<Diff> {
    let merge_base = git(cwd, &["merge-base", base, "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_else(|| base.to_string());
    Some(Diff {
        stat: git(cwd, &["diff", "--stat", &merge_base])?,
        diff: git(cwd, &["diff", &merge_base])?,
        untracked: git(cwd, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    })
}

/// The app's worker: implements `app_space_sdk::worker::Worker` so the daemon
/// can hand it dispatched items.
pub struct ClaudeCodeWorker {
    settings_path: PathBuf,
    /// This app's own base URL (`http://127.0.0.1:<port>`) — the Stop hook calls it.
    hook_base: String,
    /// Scratch root for per-run config files.
    run_root: PathBuf,
    tasks: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

impl ClaudeCodeWorker {
    pub fn new(settings_path: PathBuf, hook_base: String, run_root: PathBuf) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { settings_path, hook_base, run_root, tasks: Mutex::new(HashMap::new()) })
    }

    fn settings(&self) -> ClaudeCodeSettings {
        settings::load(&self.settings_path)
    }

    /// Run `job`. `job.timeout_secs` bounds the whole job, all rounds included.
    async fn execute(&self, job: Job, run_id: String, token: String) -> Outcome {
        let st = self.settings();
        if crate::procs::binary_version(&st.binary).await.is_none() {
            return Outcome::Blocked {
                reason: format!(
                    "claude-code: `{}` is not runnable — install Claude Code and sign in, or set the binary in app-claude settings",
                    st.binary
                ),
            };
        }
        let verify_cmd = verify::configured(st.verify_command.as_deref());
        registry::update(&run_id, |e| {
            e.verify_command = verify_cmd.clone();
        });
        let outcome = self.drive(&st, &job, &run_id, &token, verify_cmd).await;
        let _ = std::fs::remove_dir_all(self.run_root.join(&run_id));
        outcome
    }

    /// Tool allow/deny additions for the job's MCP servers.
    fn tool_rules(servers: &[JobMcp]) -> (Vec<String>, Vec<String>) {
        let allow = servers.iter().map(|s| format!("mcp__{}", s.name)).collect();
        // The source's own "close the item" tools: SenClaw closes it after verifying.
        let deny = servers
            .iter()
            .flat_map(|s| [format!("mcp__{}__kanban_complete", s.name), format!("mcp__{}__kanban_block", s.name)])
            .collect();
        (allow, deny)
    }

    async fn drive(&self, st: &ClaudeCodeSettings, job: &Job, run_id: &str, token: &str, verify_cmd: Option<String>) -> Outcome {
        let cwd = Path::new(&job.working_dir);
        let deadline = Instant::now() + Duration::from_secs(job.timeout_secs.max(1));
        let mcp = mcp_config(&job.mcp);
        let (allow_mcp, deny) = Self::tool_rules(&job.mcp);
        let mut allowed = st.allowed_tools.clone();
        allowed.extend(allow_mcp);
        let system = format!(
            "{WORKER_GUIDANCE}\n- This is SenClaw work item `{id}`; with the kanban tools, read it via \
             `kanban_show(card_id={id})`.",
            id = job.item_id
        );
        let session_id = uuid::Uuid::new_v4().to_string();
        registry::update(run_id, |e| e.session_id = Some(session_id.clone()));
        let run_dir = self.run_root.join(run_id);
        let mut prompt = job.prompt.clone();
        let mut total_cost = 0.0;
        let mut verify_note = String::from("not configured");
        let mut review_note = String::from("skipped");

        for round in 0..=st.max_fix_rounds {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Outcome::TimedOut;
            }
            registry::update(run_id, |e| {
                e.phase = "running".into();
                e.fix_round = round;
                // Each round gets its own Stop-hook budget.
                e.stop_blocks = 0;
            });
            let spec = RunSpec {
                binary: st.binary.clone(),
                cwd: cwd.to_path_buf(),
                prompt: prompt.clone(),
                session: if round == 0 { Session::New(session_id.clone()) } else { Session::Resume(session_id.clone()) },
                append_system_prompt: Some(system.clone()),
                permission_mode: st.permission_mode.clone(),
                allowed_tools: allowed.clone(),
                disallowed_tools: deny.clone(),
                model: st.model.clone(),
                max_turns: st.max_turns,
                max_budget_usd: st.max_budget_usd,
                json_schema: Some(result_schema()),
                mcp_config: mcp.clone(),
                settings: Some(crate::hooks::stop_hook_settings(&self.hook_base, token, st.verify_timeout_secs + 60)),
                run_dir: run_dir.clone(),
                timeout: remaining,
            };
            let res = match runner::run(&spec, |ev| progress(run_id, ev)).await {
                Ok(r) => r,
                Err(e) => return Outcome::Failed { error: format!("claude-code: {e:#}") },
            };
            if let Some(r) = &res.result {
                total_cost += r.cost_usd;
                let c = total_cost;
                registry::update(run_id, |e| e.cost_usd = c);
            }
            if res.timed_out {
                return Outcome::TimedOut;
            }
            if !res.succeeded() {
                return Outcome::Failed { error: res.error_text() };
            }
            let r = res.result.as_ref().expect("succeeded implies a result");
            let report = report_from(r.structured.as_ref(), &r.result);
            if report.blocked {
                return Outcome::Blocked { reason: format!("claude-code: {}", report.summary) };
            }

            // Independent checks.
            let v = match &verify_cmd {
                Some(cmd) => {
                    registry::update(run_id, |e| e.phase = "verifying".into());
                    let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(st.verify_timeout_secs));
                    let v = verify::run(cmd, cwd, left, run_id).await;
                    verify_note = if v.passed { format!("passed (`{cmd}`)") } else { format!("failed (`{cmd}`)") };
                    Some(v)
                }
                None => None,
            };
            let review = if st.review && v.as_ref().is_none_or(|v| v.passed) {
                match self.review(st, job, run_dir.clone(), deadline, run_id, &mut total_cost).await {
                    ReviewStep::Skipped(why) => {
                        review_note = format!("skipped ({why})");
                        None
                    }
                    ReviewStep::Done(rv) => {
                        review_note = format!("{} — {}", if rv.pass { "pass" } else { "changes requested" }, rv.summary);
                        Some(rv)
                    }
                    ReviewStep::Error(e) => return Outcome::Blocked { reason: format!("claude-code review could not run: {e}") },
                }
            } else {
                None
            };

            // With no verify command and nothing to review, nothing but the
            // model's own word says the work is done — a person decides.
            if v.is_none() && review.is_none() {
                return Outcome::Blocked {
                    reason: format!(
                        "review-required: claude-code reports done, but no independent check ran \
                         (no verify command in app-claude settings; review {review_note}). {}",
                        report.summary
                    ),
                };
            }
            let ok = v.as_ref().is_none_or(|v| v.passed) && review.as_ref().is_none_or(|r| r.pass);
            if ok {
                let summary = format!(
                    "{}\n\nverify: {verify_note}\nreview: {review_note}\nclaude-code session `{session_id}`, {} fix round(s), cost ${total_cost:.2}",
                    if report.summary.is_empty() { "claude-code finished" } else { &report.summary },
                    round
                );
                return Outcome::Completed {
                    summary,
                    metadata: json!({
                        "claude_code": {
                            "session_id": session_id,
                            "fix_rounds": round,
                            "cost_usd": total_cost,
                            "verify": verify_note,
                            "review": review_note,
                        }
                    }),
                };
            }
            let fb = feedback(v.as_ref(), review.as_ref());
            if round == st.max_fix_rounds {
                return Outcome::Blocked {
                    reason: format!(
                        "claude-code: checks still failing after {} fix round(s) (session `{session_id}`).\n\n{fb}",
                        st.max_fix_rounds
                    ),
                };
            }
            prompt = fb;
        }
        unreachable!("the loop returns on its last round")
    }

    async fn review(
        &self,
        st: &ClaudeCodeSettings,
        job: &Job,
        run_dir: PathBuf,
        deadline: Instant,
        run_id: &str,
        total_cost: &mut f64,
    ) -> ReviewStep {
        let cwd = Path::new(&job.working_dir);
        let Some(wt) = &job.worktree else {
            return ReviewStep::Skipped("no worktree to diff".into());
        };
        let Some(d) = diff_against(cwd, &wt.base) else {
            return ReviewStep::Skipped("git diff failed".into());
        };
        if d.diff.trim().is_empty() && d.untracked.is_empty() {
            return ReviewStep::Skipped("no changes".into());
        }
        registry::update(run_id, |e| e.phase = "reviewing".into());
        let mut diff = d.diff.clone();
        if diff.len() > REVIEW_DIFF_BYTES {
            diff.truncate(diff.floor_char_boundary(REVIEW_DIFF_BYTES));
            diff.push_str("\n… (diff truncated; read the files for the rest)");
        }
        let untracked = if d.untracked.is_empty() { String::new() } else { format!("\nUntracked files: {}", d.untracked.join(", ")) };
        let prompt = format!(
            "Review this change against its task. Read files as needed; do not modify anything.\n\n\
             ## Task\n{}\n\n## Diff (base `{}` → branch `{}`)\n{}{}\n```diff\n{}\n```\n\n\
             Pass only if the change does what the task asks, has no evident bugs or regressions, and \
             tests were not weakened. Give your verdict as the structured result.",
            job.prompt, wt.base, wt.branch, d.stat, untracked, diff
        );
        let spec = RunSpec {
            binary: st.binary.clone(),
            cwd: cwd.to_path_buf(),
            prompt,
            session: Session::New(uuid::Uuid::new_v4().to_string()),
            append_system_prompt: Some("You are a strict, read-only code reviewer for SenClaw.".into()),
            permission_mode: "plan".into(),
            allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            disallowed_tools: vec!["Edit".into(), "Write".into(), "Bash".into()],
            model: st.model.clone(),
            max_turns: 30,
            max_budget_usd: st.max_budget_usd,
            json_schema: Some(review_schema()),
            mcp_config: json!({ "mcpServers": {} }),
            settings: None,
            run_dir,
            timeout: deadline.saturating_duration_since(Instant::now()),
        };
        match runner::run(&spec, |_| {}).await {
            Ok(res) if res.succeeded() => {
                let r = res.result.as_ref().expect("succeeded implies a result");
                *total_cost += r.cost_usd;
                ReviewStep::Done(review_from(r.structured.as_ref(), &r.result))
            }
            Ok(res) => ReviewStep::Error(res.error_text()),
            Err(e) => ReviewStep::Error(format!("{e:#}")),
        }
    }
}

fn phase_of(o: &Outcome) -> &'static str {
    match o {
        Outcome::Completed { .. } => "done",
        Outcome::Blocked { .. } => "blocked",
        Outcome::Failed { .. } | Outcome::TimedOut => "failed",
    }
}

/// The `Worker` the daemon drives (a newtype: the trait and `Arc` are both foreign).
pub struct AppWorker(pub std::sync::Arc<ClaudeCodeWorker>);

#[async_trait::async_trait]
impl Worker for AppWorker {
    async fn start(&self, job: Job) -> anyhow::Result<String> {
        let st = self.0.settings();
        let (run_id, token) = registry::register(registry::NewRun {
            item_id: job.item_id.clone(),
            cwd: job.working_dir.clone(),
            verify_command: None,
            verify_timeout_secs: st.verify_timeout_secs,
            max_stop_blocks: st.max_stop_blocks,
        });
        let me = std::sync::Arc::clone(&self.0);
        let id = run_id.clone();
        let handle = tokio::spawn(async move {
            let outcome = me.execute(job, id.clone(), token).await;
            registry::finish(&id, phase_of(&outcome), outcome);
            me.tasks.lock().unwrap().remove(&id);
        });
        self.0.tasks.lock().unwrap().insert(run_id.clone(), handle.abort_handle());
        Ok(run_id)
    }

    async fn status(&self, run_id: &str) -> Option<RunStatus> {
        registry::get(run_id).map(|e| RunStatus {
            run_id: e.run_id,
            phase: e.phase,
            activity: e.last_activity,
            outcome: e.outcome,
        })
    }

    async fn cancel(&self, run_id: &str) {
        if let Some(h) = self.0.tasks.lock().unwrap().remove(run_id) {
            h.abort();
            // The whole groups — claude and whatever it started — not just the
            // direct child that kill_on_drop would reach.
            crate::procs::kill_tagged(run_id);
            if registry::get(run_id).is_some_and(|e| e.outcome.is_none()) {
                registry::finish(run_id, "failed", Outcome::Failed { error: "canceled by SenClaw".into() });
            }
        }
    }
}

enum ReviewStep {
    Skipped(String),
    Done(Review),
    Error(String),
}

fn progress(run_id: &str, ev: &StreamEvent) {
    let note = match ev {
        StreamEvent::ToolUse { name } => format!("tool: {name}"),
        StreamEvent::Text(t) => t.lines().next().unwrap_or("").chars().take(160).collect(),
        _ => return,
    };
    registry::update(run_id, |e| e.last_activity = note);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_carries_the_bridge_and_its_token() {
        let servers = vec![
            JobMcp {
                name: "senclaw-core".into(),
                url: "http://127.0.0.1:18788/mcp/core".into(),
                headers: [("Authorization".to_string(), "Bearer mdt_x".to_string())].into_iter().collect(),
            },
            JobMcp { name: "senclaw-kanban".into(), url: "http://127.0.0.1:18788/mcp/kanban".into(), headers: Default::default() },
        ];
        let v = mcp_config(&servers);
        assert_eq!(v["mcpServers"]["senclaw-core"]["url"], "http://127.0.0.1:18788/mcp/core");
        assert_eq!(v["mcpServers"]["senclaw-core"]["headers"]["Authorization"], "Bearer mdt_x");
        assert_eq!(v["mcpServers"]["senclaw-kanban"]["url"], "http://127.0.0.1:18788/mcp/kanban");
        let (allow, deny) = ClaudeCodeWorker::tool_rules(&servers);
        assert_eq!(allow, vec!["mcp__senclaw-core", "mcp__senclaw-kanban"]);
        assert!(deny.contains(&"mcp__senclaw-kanban__kanban_complete".to_string()));
    }

    #[test]
    fn report_prefers_structured_then_trailing_json_then_text() {
        let s = json!({"status": "blocked", "summary": "need the API key name"});
        assert_eq!(report_from(Some(&s), "ignored"), Report { blocked: true, summary: "need the API key name".into() });
        let t = "did it\n{\"status\":\"done\",\"summary\":\"added tests\"}";
        assert_eq!(report_from(None, t), Report { blocked: false, summary: "added tests".into() });
        assert_eq!(report_from(None, "\nall good\nmore"), Report { blocked: false, summary: "all good".into() });
    }

    #[test]
    fn an_unreadable_review_is_not_a_pass() {
        assert!(!review_from(None, "looks fine").pass);
        let v = json!({"verdict": "changes_requested", "summary": "s", "issues": ["off by one"]});
        let r = review_from(Some(&v), "");
        assert!(!r.pass && r.issues == vec!["off by one".to_string()]);
        assert!(review_from(Some(&json!({"verdict": "pass", "summary": "ok"})), "").pass);
    }

    #[test]
    fn feedback_names_every_failed_check() {
        let v = VerifyResult { command: "cargo test".into(), passed: false, exit_code: Some(101), timed_out: false, output_tail: "panicked".into() };
        let r = Review { pass: false, summary: "missing edge case".into(), issues: vec!["empty input".into()] };
        let f = feedback(Some(&v), Some(&r));
        assert!(f.contains("panicked") && f.contains("missing edge case") && f.contains("- empty input"));
        let passed = VerifyResult { passed: true, ..v };
        assert!(!feedback(Some(&passed), None).contains("Verification failed"));
    }
}
