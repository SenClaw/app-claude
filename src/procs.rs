//! Process helpers shared by the Claude runner and the verify command.

/// SIGKILL a whole process group (the child was spawned with
/// `process_group(0)`): Bash commands, dev servers and MCP children a session
/// started die with it instead of outliving the run.
pub fn kill_group(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pgid {
        // SAFETY: plain syscall on a pid we spawned; a stale group is ESRCH.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Live process groups, tagged with the run they belong to. The supervisor
/// stops an app by signalling the app's own group, which never reaches these
/// (each child leads its own group) — so shutdown and cancel kill them here.
fn groups() -> &'static std::sync::Mutex<Vec<(u32, String)>> {
    static G: std::sync::OnceLock<std::sync::Mutex<Vec<(u32, String)>>> = std::sync::OnceLock::new();
    G.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Registered while alive; dropping it unregisters the group.
pub struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            groups().lock().unwrap().retain(|(g, _)| *g != p);
        }
    }
}

/// Track `pgid` as belonging to run `tag`.
pub fn track(pgid: Option<u32>, tag: &str) -> GroupGuard {
    if let Some(p) = pgid {
        groups().lock().unwrap().push((p, tag.to_string()));
    }
    GroupGuard(pgid)
}

/// Kill every group of run `tag`.
pub fn kill_tagged(tag: &str) {
    let pgids: Vec<u32> = groups().lock().unwrap().iter().filter(|(_, t)| t == tag).map(|(p, _)| *p).collect();
    for p in pgids {
        kill_group(Some(p));
    }
}

/// Kill every tracked group (app shutdown).
pub fn kill_all() {
    let pgids: Vec<u32> = groups().lock().unwrap().iter().map(|(p, _)| *p).collect();
    for p in pgids {
        kill_group(Some(p));
    }
}

/// Read `r` to EOF keeping only the last `cap` bytes — bounded memory no
/// matter how much a command prints.
pub async fn read_tail<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> String {
    use tokio::io::AsyncReadExt;
    let mut buf: std::collections::VecDeque<u8> = std::collections::VecDeque::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend(&chunk[..n]);
                let over = buf.len().saturating_sub(cap);
                buf.drain(..over);
            }
        }
    }
    String::from_utf8_lossy(buf.make_contiguous()).into_owned()
}

/// `<binary> --version`, or `None` when the binary cannot run.
pub async fn binary_version(binary: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Owner-only permissions (the file may carry a run token).
pub fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Constant-time string compare (tokens).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <token>`.
pub fn bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_tail_keeps_only_the_end() {
        let data = b"0123456789".repeat(1000);
        assert_eq!(read_tail(&data[..], 15).await, "567890123456789");
    }

    #[tokio::test]
    async fn a_missing_binary_has_no_version() {
        assert!(binary_version("app-claude-definitely-not-a-binary").await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_a_runs_groups_stops_its_children() {
        let mut child = tokio::process::Command::new("sleep").arg("30").process_group(0).kill_on_drop(true).spawn().unwrap();
        let guard = track(child.id(), "run-k");
        kill_tagged("run-k");
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
        assert!(!status.success());
        drop(guard);
        assert!(groups().lock().unwrap().iter().all(|(_, t)| t != "run-k"));
    }

    #[test]
    fn bearer_tokens_are_read_and_compared() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", "Bearer ccr_x".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("ccr_x"));
        assert!(ct_eq("ab", "ab") && !ct_eq("ab", "ac") && !ct_eq("a", "ab"));
    }
}
