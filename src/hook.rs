//! The `Stop` hook of a session: once the session's issue (or the PR it
//! reviews) is at the end of the session's part, the session exits on its
//! own instead of idling until issuewright closes its window.
//!
//! Claude Code runs the hook after every turn, outside the sandbox and with
//! the session's environment. It reads the session's lock to know what the
//! session works on, asks GitHub for the subject's state and labels, and
//! closes the session's tmux window when the session is finished. issuewright then
//! sees the window gone at its next poll and frees the slot, as it does for
//! any ended session.
//!
//! A turn that ends while the session still has work means it waits for an
//! answer (interactive mode). The hook then marks the slot as waiting, and the
//! `UserPromptSubmit` hook clears the mark once anyone answers; issuewright uses the
//! mark for `question_timeout_secs`, and `issuewright watch` shows it.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::daemon::log_line;
use crate::github::Gh;
use crate::model::*;
use crate::state::{Kind, Mode, State, Waiting};
use crate::tmux;

/// Why a session of `kind` is finished, judged from its subject's state and
/// labels right after one of its turns. `None` while it still has work.
///
/// - An implement or epic-check session is finished once its issue left
///   `dispatch:running`: it went to `dispatch:review` (PR up, CI green),
///   `dispatch:needs-input` (a question, or a dead end), or was split.
/// - A reviewer session is finished at its verdict (approval, changes
///   requested, or handed back to you), or once the subject left
///   `dispatch:review`.
/// - Any session is finished once its subject is closed.
pub fn finished(kind: Kind, open: bool, labels: &[String]) -> Option<&'static str> {
    let has = |l: &str| labels.iter().any(|x| x.eq_ignore_ascii_case(l));
    if !open {
        return Some("its issue is closed");
    }
    match kind {
        Kind::Implement | Kind::EpicCheck => {
            if has(CI) {
                Some("its issue is dispatch:ci")
            } else if has(REVIEW) {
                Some("its issue is dispatch:review")
            } else if has(NEEDS_INPUT) {
                Some("its issue is dispatch:needs-input")
            } else if !has(RUNNING) {
                Some("its issue is no longer dispatch:running")
            } else {
                None
            }
        }
        Kind::Review => {
            if has(APPROVED) || has(MERGING) {
                Some("it approved the PR")
            } else if has(NEEDS_INPUT) || has(CHANGES) {
                Some("it handed the PR back")
            } else if !has(REVIEW) {
                Some("its subject is no longer dispatch:review")
            } else {
                None
            }
        }
    }
}

/// `issuewright stop-hook`: closes the window of the session in `repo`'s `slot`
/// when that session is finished. Errors leave the session running; issuewright's
/// own poll still closes it later.
pub fn run(cfg: &Config, repo: &str, slot: usize) -> Result<()> {
    let state = State::open(cfg.state_dir())?;
    let Some(lock) = state
        .locks()?
        .into_iter()
        .find(|l| l.repo.eq_ignore_ascii_case(repo) && l.slot == slot)
    else {
        return Ok(());
    };
    let Some(window) = &lock.tmux_window else {
        return Ok(());
    };
    let gh = match std::env::var("GH_TOKEN") {
        Ok(t) if !t.is_empty() => Gh::with_token(t),
        _ => Gh::new(cfg.gh_user.as_deref())?,
    };
    let (open, labels) = gh
        .state_and_labels(&lock.issue)?
        .with_context(|| format!("{} not found", lock.issue))?;
    let Some(why) = finished(lock.kind, open, &labels) else {
        if lock.mode == Mode::Interactive {
            let since = state.waiting(repo, slot).map(|w| w.since);
            if !since.is_some_and(|t| t >= lock.started_at) {
                state.set_waiting(
                    repo,
                    slot,
                    &Waiting {
                        since: chrono::Utc::now(),
                        nudged_at: None,
                    },
                )?;
            }
        }
        return Ok(());
    };
    log_line(
        &cfg.logs_dir().join("issuewright.log"),
        &format!("{}: finished ({why}), the session exits", lock.name),
    );
    tmux::kill_window(window)
}

/// `issuewright prompt-hook`: someone answered the session in `repo`'s `slot`.
pub fn answered(cfg: &Config, repo: &str, slot: usize) -> Result<()> {
    State::open(cfg.state_dir())?.clear_waiting(repo, slot)
}

/// A hook's command line (`stop-hook` or `prompt-hook`), run by Claude Code
/// through a shell.
pub fn command(exe: &Path, config: &Path, hook: &str, repo: &str, slot: usize) -> String {
    format!(
        "{} --config {} {hook} --repo {} --slot {slot}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&config.to_string_lossy()),
        shell_quote(repo)
    )
}

/// The running issuewright binary. After `cargo install` replaced it, Linux
/// reports the old file as `<path> (deleted)`; the new one is at `<path>`.
pub fn current_exe() -> Result<std::path::PathBuf> {
    let p = std::env::current_exe().context("finding issuewright's own binary")?;
    let s = p.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(live) => live.into(),
        None => p,
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn implement_session_finishes_when_it_leaves_running() {
        let k = Kind::Implement;
        assert_eq!(finished(k, true, &l(&[RUNNING, "type:story"])), None);
        assert!(finished(k, true, &l(&[CI])).is_some());
        assert!(finished(k, true, &l(&[REVIEW])).is_some());
        assert!(finished(k, true, &l(&[NEEDS_INPUT])).is_some());
        // Split into sub-issues: dispatch:running removed, nothing added.
        assert!(finished(k, true, &l(&["type:story"])).is_some());
        assert!(finished(k, false, &l(&[RUNNING])).is_some());
    }

    #[test]
    fn epic_check_finishes_when_it_leaves_running() {
        let k = Kind::EpicCheck;
        assert_eq!(finished(k, true, &l(&[RUNNING])), None);
        assert!(finished(k, true, &l(&[NEEDS_INPUT])).is_some());
        assert!(finished(k, true, &l(&[])).is_some());
        assert!(finished(k, false, &l(&[RUNNING])).is_some());
    }

    #[test]
    fn review_session_finishes_at_its_verdict() {
        let k = Kind::Review;
        assert_eq!(finished(k, true, &l(&[REVIEW])), None);
        assert!(finished(k, true, &l(&[REVIEW, APPROVED])).is_some());
        assert!(finished(k, true, &l(&[NEEDS_INPUT])).is_some());
        assert!(finished(k, true, &l(&[MERGING])).is_some());
        assert!(finished(k, true, &l(&[REVIEW, CHANGES])).is_some());
        // Merge later: released to wait on the blockers it recorded.
        assert!(finished(k, true, &l(&[READY])).is_some());
        assert!(finished(k, false, &l(&[REVIEW])).is_some());
    }

    #[test]
    fn command_quotes_paths() {
        assert_eq!(
            command(
                Path::new("/opt/my bin/issuewright"),
                Path::new("/home/u/it's.toml"),
                "stop-hook",
                "acme/app",
                2
            ),
            r"'/opt/my bin/issuewright' --config '/home/u/it'\''s.toml' stop-hook --repo 'acme/app' --slot 2"
        );
    }
}
