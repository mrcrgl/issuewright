//! tmux: the `agents` session, one window per interactive session.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

fn tmux(args: &[&str]) -> Result<std::process::Output> {
    Command::new("tmux")
        .args(args)
        .output()
        .context("running tmux")
}

pub fn has_session(session: &str) -> bool {
    tmux(&["has-session", "-t", &format!("={session}")])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Creates the session with window 0 following issuewright's log, unless it exists.
pub fn ensure_session(session: &str, log: &Path) -> Result<()> {
    if has_session(session) {
        return Ok(());
    }
    let log = log.to_string_lossy();
    let out = tmux(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-n",
        "issuewright",
        "tail",
        "-n",
        "200",
        "-F",
        &log,
    ])?;
    if !out.status.success() {
        bail!(
            "tmux new-session: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Opens a detached window running `argv` (no shell). Returns the window id
/// and the pid of the process in its pane.
pub fn new_window(session: &str, name: &str, cwd: &Path, argv: &[String]) -> Result<(String, u32)> {
    let target = format!("={session}:");
    let cwd = cwd.to_string_lossy();
    let mut args: Vec<&str> = vec![
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id} #{pane_pid}",
        "-t",
        &target,
        "-n",
        name,
        "-c",
        &cwd,
    ];
    args.extend(argv.iter().map(String::as_str));
    let out = tmux(&args)?;
    if !out.status.success() {
        bail!(
            "tmux new-window: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let id = parts
        .next()
        .context("tmux printed no window id")?
        .to_string();
    let pid = parts
        .next()
        .and_then(|p| p.parse().ok())
        .context("tmux printed no pane pid")?;
    // A window whose process exits must close, so its slot gets freed.
    let _ = tmux(&["set-option", "-w", "-t", &id, "remain-on-exit", "off"]);
    let _ = tmux(&["set-option", "-w", "-t", &id, "automatic-rename", "off"]);
    Ok((id, pid))
}

/// Closes a window; its process gets SIGHUP. A window that is already gone
/// is not an error.
pub fn kill_window(id: &str) -> Result<()> {
    let out = tmux(&["kill-window", "-t", id])?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success()
        && !err.contains("can't find window")
        && !err.contains("no server running")
    {
        bail!("tmux kill-window {id}: {}", err.trim());
    }
    Ok(())
}

/// Ids of all windows of all sessions, or `None` when tmux isn't reachable.
pub fn window_ids() -> Option<BTreeSet<String>> {
    let out = tmux(&["list-windows", "-a", "-F", "#{window_id}"]).ok()?;
    if !out.status.success() {
        // No server running: no windows.
        let err = String::from_utf8_lossy(&out.stderr);
        return (err.contains("no server running") || err.contains("No such file"))
            .then(BTreeSet::new);
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(String::from)
            .collect(),
    )
}

/// Types `text` into the window's active pane and presses Enter, as if
/// someone typed an answer. `text` must be one line.
pub fn send_line(window: &str, text: &str) -> Result<()> {
    let out = tmux(&["send-keys", "-t", window, "-l", text])?;
    if !out.status.success() {
        bail!(
            "tmux send-keys {window}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Claude Code takes a fast burst of keys as a paste; let it settle before
    // the Enter that submits it.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = tmux(&["send-keys", "-t", window, "Enter"])?;
    if !out.status.success() {
        bail!(
            "tmux send-keys {window} Enter: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Shows `window` in the tmux client this process runs in. Fails outside
/// tmux.
pub fn switch_to(window: &str) -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("not inside tmux");
    }
    let out = tmux(&["display-message", "-p", "-t", window, "#{session_name}"])?;
    if !out.status.success() {
        bail!("window {window} is gone");
    }
    let session = String::from_utf8_lossy(&out.stdout).trim().to_string();
    tmux(&["select-window", "-t", window])?;
    let out = tmux(&["switch-client", "-t", &format!("={session}")])?;
    if !out.status.success() {
        bail!(
            "tmux switch-client: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
