//! Independent verification: SenClaw runs the project's own check command in
//! the worker's directory and trusts the exit code, never the model's report.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// How much combined output a failure report carries back to Claude / the card.
const OUTPUT_TAIL: usize = 6000;

#[derive(Debug, Clone, PartialEq)]
pub struct VerifyResult {
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Tail of stdout+stderr.
    pub output_tail: String,
}

impl VerifyResult {
    /// Feedback text for Claude or a card comment.
    pub fn report(&self) -> String {
        let status = if self.timed_out {
            "timed out".to_string()
        } else {
            format!("exit {}", self.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()))
        };
        format!("`{}` {}:\n```\n{}\n```", self.command, status, self.output_tail.trim_end())
    }
}

/// The configured verify command. Deliberately never read from the card: card
/// text can come from a remote peer or a chat, and must not choose what runs
/// in a shell.
pub fn configured(default: Option<&str>) -> Option<String> {
    default.map(str::trim).filter(|c| !c.is_empty()).map(str::to_string)
}

/// Run `command` through the platform shell in `cwd`, its process group
/// tracked under run `tag` (so a cancel or shutdown kills it).
pub async fn run(command: &str, cwd: &Path, timeout: Duration, tag: &str) -> VerifyResult {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Both streams into one bounded tail.
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return VerifyResult {
                command: command.into(),
                passed: false,
                exit_code: None,
                timed_out: false,
                output_tail: format!("could not start: {e}"),
            }
        }
    };
    let pgid = child.id();
    let _group = crate::procs::track(pgid, tag);
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let collect = async {
        let (o, e) = tokio::join!(
            async { match out { Some(r) => crate::procs::read_tail(r, OUTPUT_TAIL).await, None => String::new() } },
            async { match err { Some(r) => crate::procs::read_tail(r, OUTPUT_TAIL).await, None => String::new() } },
        );
        let status = child.wait().await;
        (o, e, status)
    };
    let result = tokio::time::timeout(timeout, collect).await;
    // Leftovers (watchers, servers started by the check) end with it.
    crate::procs::kill_group(pgid);
    match result {
        Ok((o, e, Ok(status))) => {
            let mut text = o;
            text.push_str(&e);
            let start = text.len().saturating_sub(OUTPUT_TAIL);
            VerifyResult {
                command: command.into(),
                passed: status.success(),
                exit_code: status.code(),
                timed_out: false,
                output_tail: text[text.floor_char_boundary(start)..].to_string(),
            }
        }
        Ok((_, _, Err(e))) => VerifyResult {
            command: command.into(),
            passed: false,
            exit_code: None,
            timed_out: false,
            output_tail: format!("wait failed: {e}"),
        },
        Err(_) => VerifyResult {
            command: command.into(),
            passed: false,
            exit_code: None,
            timed_out: true,
            output_tail: format!("no result after {}s", timeout.as_secs()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_configured_command_counts() {
        assert_eq!(configured(Some(" make ")), Some("make".into()));
        assert_eq!(configured(Some("")), None);
        assert_eq!(configured(None), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_code_decides_and_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run("echo fine", dir.path(), Duration::from_secs(10), "t").await;
        assert!(ok.passed);
        let bad = run("echo broken >&2; exit 3", dir.path(), Duration::from_secs(10), "t").await;
        assert!(!bad.passed);
        assert_eq!(bad.exit_code, Some(3));
        assert!(bad.report().contains("broken") && bad.report().contains("exit 3"));
        let slow = run("sleep 5", dir.path(), Duration::from_millis(200), "t").await;
        assert!(slow.timed_out && !slow.passed);
        let loud = run("yes | head -c 200000", dir.path(), Duration::from_secs(10), "t").await;
        assert!(loud.passed && loud.output_tail.len() <= OUTPUT_TAIL);
    }
}
