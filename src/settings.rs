//! The app's settings file (`settings.json` in its data directory). Read per
//! run (no restart); anything missing or malformed falls back to the default.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeCodeSettings {
    /// The `claude` executable — a bare name is looked up on `PATH`.
    pub binary: String,
    /// `--model`; `None` = the user's Claude Code default.
    pub model: Option<String>,
    /// `--permission-mode` for the worker session. The worker always runs in
    /// the card's directory (a git worktree when the card asks for one).
    pub permission_mode: String,
    /// `--allowedTools` for the worker session. Server names of the MCP
    /// servers SenClaw injects are added automatically.
    pub allowed_tools: Vec<String>,
    /// `--max-turns` per session.
    pub max_turns: u32,
    /// `--max-budget-usd` per session (API-key billing only).
    pub max_budget_usd: Option<f64>,
    /// Shell command that must exit 0 before a card may close (`cargo test`,
    /// `npm test`, …). Only ever read from here — never from card text.
    pub verify_command: Option<String>,
    pub verify_timeout_secs: u64,
    /// How many times the Stop hook may send Claude back to fix a failing
    /// verify before it is let go (the post-run verify still decides).
    pub max_stop_blocks: u32,
    /// Run a read-only reviewer session over the diff before closing.
    pub review: bool,
    /// Feedback rounds (`--resume` with verify/review findings) before the
    /// card is blocked for a person.
    pub max_fix_rounds: u32,
}

impl Default for ClaudeCodeSettings {
    fn default() -> Self {
        Self {
            binary: "claude".into(),
            model: None,
            permission_mode: "acceptEdits".into(),
            allowed_tools: vec!["Bash".into(), "Read".into(), "Edit".into(), "Write".into()],
            max_turns: 80,
            max_budget_usd: None,
            verify_command: None,
            verify_timeout_secs: 900,
            max_stop_blocks: 3,
            review: true,
            max_fix_rounds: 2,
        }
    }
}

impl ClaudeCodeSettings {
    /// Reject values that would make a run meaningless or unsafe to start.
    pub fn validated(self) -> Result<Self, String> {
        const MODES: &[&str] = &["default", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"];
        if self.binary.trim().is_empty() {
            return Err("binary: must not be empty".into());
        }
        if !MODES.contains(&self.permission_mode.as_str()) {
            return Err(format!("permissionMode: one of {}", MODES.join(", ")));
        }
        if self.max_turns == 0 {
            return Err("maxTurns: must be at least 1".into());
        }
        if self.verify_timeout_secs == 0 {
            return Err("verifyTimeoutSecs: must be at least 1".into());
        }
        Ok(self)
    }
}

/// Read the settings file.
pub fn load(path: &Path) -> ClaudeCodeSettings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the settings file (owner-only).
pub fn save(path: &Path, s: &ClaudeCodeSettings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(s)?)?;
    crate::procs::restrict(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_garbage_block_is_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        assert_eq!(load(&path), ClaudeCodeSettings::default());
        std::fs::write(&path, "7").unwrap();
        assert_eq!(load(&path), ClaudeCodeSettings::default());
        std::fs::write(&path, r#"{"verifyCommand": "npm test", "maxTurns": 5}"#).unwrap();
        let s = load(&path);
        assert_eq!(s.verify_command.as_deref(), Some("npm test"));
        assert_eq!(s.max_turns, 5);
        assert_eq!(s.permission_mode, "acceptEdits");
        save(&path, &s).unwrap();
        assert_eq!(load(&path), s);
    }

    #[test]
    fn unknown_permission_mode_is_refused() {
        let mut s = ClaudeCodeSettings::default();
        s.permission_mode = "yolo".into();
        assert!(s.validated().is_err());
    }
}
