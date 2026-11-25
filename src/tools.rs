//! Commands a session runs outside its sandbox: `issuewright session …`. Each one
//! does a single fixed thing to the session's own slot, which the sandbox
//! or the deny rules keep the session from doing itself:
//!
//! - Rebasing onto the default branch writes whatever files the default
//!   branch changed, and the sandbox never lets a session write some of them,
//!   such as `.claude/skills/`. Inside the sandbox the rebase fails halfway
//!   and leaves the worktree half changed.
//! - `gh run rerun` is denied, because a re-run can deploy. A re-run of the
//!   failed jobs of a run of the session's own branch can't.
//!
//! The session runs these as `<issuewright> session <command>`; issuewright lists that
//! command in the sandbox's `excludedCommands` and in the allow rules.
//!
//! The session writes that command line, arguments and environment
//! included, so none of them are trusted. The session is found by its key
//! (`ISSUEWRIGHT_SESSION_KEY`, set when issuewright starts it) in a record issuewright
//! writes under the user's home as the user database names it, in a directory every
//! session's sandbox denies reading and writing. git runs in the slot the
//! record names, with a fixed environment and without hooks.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::Kind;

/// The variable that carries a session's key.
pub const KEY_ENV: &str = "ISSUEWRIGHT_SESSION_KEY";

/// Re-runs of a CI run beyond this attempt are refused: a run that failed
/// this often is no flake.
const MAX_RUN_ATTEMPT: u64 = 3;

#[derive(Subcommand, Debug)]
pub enum Tool {
    /// Fetch and rebase this branch onto the default branch. Stops at a
    /// conflict: resolve it, `git add` the files, then `continue`.
    Rebase,
    /// Continue the rebase after resolving its conflicts.
    Continue,
    /// Abort the rebase; the branch is as it was.
    Abort,
    /// Discard every uncommitted change to tracked and untracked files (not
    /// ignored ones, such as `target/`), and abort a rebase in progress.
    Reset,
    /// Re-run the failed jobs of a finished CI run of this branch.
    RerunCi {
        /// The run's id, from `gh run list` or the run's URL.
        run: u64,
    },
}

/// What issuewright records about a session it starts, for its tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub repo: String,
    pub slot: usize,
    pub dir: PathBuf,
    pub branch: String,
    pub kind: Kind,
}

/// Where the records live: `~/.local/state/issuewright/sessions`, with the home
/// directory from the user database, not `$HOME`.
pub fn records_dir() -> Result<PathBuf> {
    Ok(user_home()?.join(".local/state/issuewright/sessions"))
}

fn record_path(dir: &Path, repo: &str, slot: usize) -> PathBuf {
    dir.join(format!("{}-{slot}.json", repo.replace('/', "-")))
}

/// Writes the record of a session about to start; it replaces the record of
/// the slot's previous session.
pub fn write_record(r: &Record) -> Result<()> {
    let dir = records_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let p = record_path(&dir, &r.repo, r.slot);
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(r)?)?;
    std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))
}

/// The record whose key is `key`.
fn find_record(dir: &Path, key: &str) -> Result<Record> {
    if key.len() < 16 {
        bail!("not run by an issuewright session (${KEY_ENV} is not set)");
    }
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in entries.flatten() {
        if e.path().extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if let Ok(r) = serde_json::from_str::<Record>(&text)
            && r.key == key
        {
            return Ok(r);
        }
    }
    bail!(
        "no session issuewright started has this ${KEY_ENV}; only the session issuewright started last in a slot can use these commands"
    )
}

/// `issuewright session <tool>`.
pub fn run(tool: Tool) -> Result<()> {
    let key = std::env::var(KEY_ENV).unwrap_or_default();
    let rec = find_record(&records_dir()?, &key)?;
    if rec.kind != Kind::Implement {
        bail!("only a session that implements an issue can use these commands");
    }
    let git = Git::new(&rec)?;
    match tool {
        Tool::Rebase => rebase(&git),
        Tool::Continue => {
            if !git.rebasing() {
                bail!("no rebase is in progress");
            }
            finish_rebase(&git, git.run(&["rebase", "--continue"])?)
        }
        Tool::Abort => {
            if !git.rebasing() {
                bail!("no rebase is in progress");
            }
            git.ok(&["rebase", "--abort"])?;
            println!("Rebase aborted; the branch is as it was before.");
            Ok(())
        }
        Tool::Reset => reset(&git),
        Tool::RerunCi { run } => rerun_ci(&rec, run),
    }
}

/// git in the session's slot: a fixed environment (the caller's could point
/// git at other config or programs), the user's own home from the user database, and
/// no hooks or fsmonitor (the session can write the shared git directory).
struct Git {
    bin: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    ssh_auth_sock: Option<String>,
    gh_token: Option<String>,
}

impl Git {
    fn new(rec: &Record) -> Result<Self> {
        Ok(Self {
            bin: trusted_bin("git")?,
            dir: rec.dir.clone(),
            home: user_home()?,
            // Commit signing with an SSH key goes through the agent.
            ssh_auth_sock: std::env::var("SSH_AUTH_SOCK").ok(),
            // Fetching uses the session's own token, through gh's
            // credential helper.
            gh_token: std::env::var("GH_TOKEN").ok(),
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.env_clear()
            .env("PATH", TRUSTED_PATH)
            .env("HOME", &self.home)
            .env("LANG", "C.UTF-8")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GH_PROMPT_DISABLED", "1");
        if let Some(s) = &self.ssh_auth_sock {
            c.env("SSH_AUTH_SOCK", s);
        }
        if let Some(t) = &self.gh_token {
            c.env("GH_TOKEN", t);
        }
        c.arg("-C")
            .arg(&self.dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args);
        c
    }

    /// Runs git; its output, both streams, goes to the caller's.
    fn run(&self, args: &[&str]) -> Result<bool> {
        let status = self
            .command(args)
            .status()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        Ok(status.success())
    }

    fn ok(&self, args: &[&str]) -> Result<()> {
        if !self.run(args)? {
            bail!("git {} failed", args.join(" "));
        }
        Ok(())
    }

    /// Runs git and returns its standard output.
    fn output(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command(args)
            .output()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn git_dir(&self) -> Result<PathBuf> {
        let p = PathBuf::from(self.output(&["rev-parse", "--git-dir"])?);
        Ok(if p.is_absolute() { p } else { self.dir.join(p) })
    }

    fn rebasing(&self) -> bool {
        self.git_dir()
            .is_ok_and(|d| d.join("rebase-merge").exists() || d.join("rebase-apply").exists())
    }
}

fn rebase(git: &Git) -> Result<()> {
    if git.rebasing() {
        bail!(
            "a rebase is in progress: resolve its conflicts and `git add` them, then `issuewright session continue`; or `issuewright session abort`"
        );
    }
    let dirty = git.output(&["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        bail!(
            "the worktree has uncommitted changes:\n{dirty}\nCommit them first, or discard them with `issuewright session reset`"
        );
    }
    git.ok(&["fetch", "--quiet", "origin"])?;
    let base = default_branch(git)?;
    let onto = format!("origin/{base}");
    let behind = git.output(&["rev-list", "--count", &format!("HEAD..{onto}")])?;
    if behind == "0" {
        println!("Already up to date with {onto}; nothing to rebase.");
        return Ok(());
    }
    println!("Rebasing onto {onto} ({behind} new commits there).");
    let done = git.run(&["rebase", &onto])?;
    finish_rebase(git, done)
}

/// Reports how a `rebase` or `rebase --continue` ended.
fn finish_rebase(git: &Git, done: bool) -> Result<()> {
    if done && !git.rebasing() {
        let head = git.output(&["log", "--oneline", "-1"])?;
        println!(
            "Rebased; HEAD is {head}. Build and test, then push with `git push --force-with-lease`."
        );
        return Ok(());
    }
    if git.rebasing() {
        let conflicts = git.output(&["diff", "--name-only", "--diff-filter=U"])?;
        bail!(
            "the rebase stopped at a conflict in:\n{conflicts}\nResolve them, `git add` the files, then `issuewright session continue`; or `issuewright session abort`"
        );
    }
    bail!("the rebase failed (see git's output above); the branch is as it was")
}

fn default_branch(git: &Git) -> Result<String> {
    let head = match git.output(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        Ok(h) => h,
        Err(_) => {
            git.ok(&["remote", "set-head", "origin", "--auto"])?;
            git.output(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])?
        }
    };
    Ok(head.trim_start_matches("origin/").to_string())
}

fn reset(git: &Git) -> Result<()> {
    if git.rebasing() {
        git.ok(&["rebase", "--abort"])?;
    }
    if git.git_dir()?.join("MERGE_HEAD").exists() {
        git.ok(&["merge", "--abort"])?;
    }
    git.ok(&["reset", "--quiet", "--hard", "HEAD"])?;
    git.ok(&["clean", "--quiet", "-fd"])?;
    let head = git.output(&["log", "--oneline", "-1"])?;
    println!("The worktree is clean at {head}.");
    Ok(())
}

/// What a re-run is allowed for: a finished, failed run of the session's
/// own branch (not the default branch), triggered by a push to it or by its
/// pull request.
fn check_rerun(run: &Value, branch: &str, default_branch: &str) -> Result<()> {
    let s = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or_default();
    let event = s("event");
    let head = s("head_branch");
    if head != branch || branch == default_branch {
        bail!("the run is of branch `{head}`, not of this session's branch `{branch}`");
    }
    if !matches!(event, "pull_request" | "push") {
        bail!("the run was triggered by `{event}`; only pull_request and push runs can be re-run");
    }
    if s("status") != "completed" {
        bail!("the run hasn't finished yet (status `{}`)", s("status"));
    }
    let conclusion = s("conclusion");
    if !matches!(
        conclusion,
        "failure" | "cancelled" | "timed_out" | "startup_failure"
    ) {
        bail!("the run didn't fail (conclusion `{conclusion}`)");
    }
    let attempt = run.get("run_attempt").and_then(Value::as_u64).unwrap_or(1);
    if attempt >= MAX_RUN_ATTEMPT {
        bail!(
            "the run already ran {attempt} times; a failure that persists isn't a flake. Find the cause, or ask"
        );
    }
    Ok(())
}

fn rerun_ci(rec: &Record, run: u64) -> Result<()> {
    let token = std::env::var("GH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .context("GH_TOKEN is not set")?;
    let http = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent("issuewright")
        .build();
    let api = |method: &str, path: &str| {
        http.request(method, &format!("https://api.github.com/{path}"))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
    };
    let repo: Value = api("GET", &format!("repos/{}", rec.repo))
        .call()
        .map_err(http_error)?
        .into_json()?;
    let default = repo["default_branch"].as_str().unwrap_or("main");
    let path = format!("repos/{}/actions/runs/{run}", rec.repo);
    let r: Value = api("GET", &path).call().map_err(http_error)?.into_json()?;
    check_rerun(&r, &rec.branch, default)?;
    api("POST", &format!("{path}/rerun-failed-jobs"))
        .call()
        .map_err(http_error)
        .context("re-running the failed jobs")?;
    let attempt = r["run_attempt"].as_u64().unwrap_or(1) + 1;
    println!(
        "Re-running the failed jobs of run {run} ({}), attempt {attempt}: {}",
        r["name"].as_str().unwrap_or("?"),
        r["html_url"].as_str().unwrap_or_default()
    );
    Ok(())
}

fn http_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {code}: {}", body.trim())
        }
        other => other.into(),
    }
}

/// The user's home directory as the user database names it (`getpwuid_r`:
/// `/etc/passwd` on Linux, Directory Services on macOS), never `$HOME`.
pub fn user_home() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: an all-zero passwd is a valid value to be filled in.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: pwd, buf and found are valid for the call, and buf.len()
        // is the buffer's real length.
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
            bail!("uid {uid} has no entry in the user database");
        }
        // SAFETY: pw_dir points into buf, NUL-terminated, while buf lives.
        let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
        return Ok(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())));
    }
}

/// Where the commands run outside the sandbox come from: system and package
/// manager directories, which sessions can't write. The session's `PATH`
/// isn't used.
const TRUSTED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

/// The first `name` in the trusted directories.
fn trusted_bin(name: &str) -> Result<PathBuf> {
    TRUSTED_PATH
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .with_context(|| format!("no {name} in {TRUSTED_PATH}"))
}

/// What a session is told about these commands, appended to its system
/// prompt. `exe` is issuewright's binary as the session must write it.
pub fn system_prompt(exe: &str) -> String {
    format!(
        "issuewright, which started this session, gives it commands that the sandbox and the deny rules otherwise keep from you. \
Run each as a command of its own: no `cd`, no `&&` or pipes, no redirects, or the sandbox still applies.\n\
- `{exe} session rebase`: fetch and rebase this branch onto the default branch. Use it instead of `git rebase`, which fails in the sandbox once the default branch changed files under `.claude/`. At a conflict it stops: resolve, `git add` the files, then `{exe} session continue`; or `{exe} session abort`.\n\
- `{exe} session reset`: discard every uncommitted change, untracked files too, and abort a rebase in progress. For a worktree a failed git command left half changed.\n\
- `{exe} session rerun-ci <run-id>`: re-run the failed jobs of a finished CI run of this branch, when the failure isn't caused by this branch (a runner that ran out of disk, a flaky test, a check that also fails on the default branch). Say on the issue why you re-ran it. It refuses runs of other branches, and after three attempts.\n\
Push yourself afterwards, as a command of its own: `git push --force-with-lease` after a rebase."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(dir: &Path, repo: &str, slot: usize, key: &str) -> Record {
        let r = Record {
            key: key.into(),
            repo: repo.into(),
            slot,
            dir: dir.join("slot"),
            branch: "feat/1-x".into(),
            kind: Kind::Implement,
        };
        std::fs::write(
            record_path(dir, repo, slot),
            serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
        r
    }

    #[test]
    fn a_session_is_found_by_its_key_only() {
        let d = std::env::temp_dir().join(format!("issuewright-tools-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = rec(&d, "o/a", 1, "aaaaaaaa-1111-2222-3333-444444444444");
        let b = rec(&d, "o/a", 2, "bbbbbbbb-1111-2222-3333-444444444444");
        assert_eq!(find_record(&d, &a.key).unwrap(), a);
        assert_eq!(find_record(&d, &b.key).unwrap(), b);
        assert!(find_record(&d, "cccccccc-1111-2222-3333-444444444444").is_err());
        assert!(find_record(&d, "").is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_failed_runs_of_the_own_branch_rerun() {
        let run = |event: &str, branch: &str, status: &str, conclusion: &str, attempt: u64| {
            json!({"event": event, "head_branch": branch, "status": status,
                   "conclusion": conclusion, "run_attempt": attempt})
        };
        let ok = |v: Value| check_rerun(&v, "feat/1-x", "master");
        assert!(ok(run("pull_request", "feat/1-x", "completed", "failure", 1)).is_ok());
        assert!(ok(run("push", "feat/1-x", "completed", "cancelled", 2)).is_ok());
        assert!(ok(run("pull_request", "feat/2-y", "completed", "failure", 1)).is_err());
        assert!(ok(run("push", "master", "completed", "failure", 1)).is_err());
        assert!(
            ok(run(
                "workflow_dispatch",
                "feat/1-x",
                "completed",
                "failure",
                1
            ))
            .is_err()
        );
        assert!(ok(run("pull_request", "feat/1-x", "in_progress", "", 1)).is_err());
        assert!(ok(run("pull_request", "feat/1-x", "completed", "success", 1)).is_err());
        assert!(ok(run("pull_request", "feat/1-x", "completed", "failure", 3)).is_err());
        // A session on the default branch itself can't re-run its runs.
        assert!(
            check_rerun(
                &run("push", "master", "completed", "failure", 1),
                "master",
                "master"
            )
            .is_err()
        );
    }

    #[test]
    fn home_comes_from_the_user_database() {
        let home = user_home().unwrap();
        assert!(home.is_absolute(), "{}", home.display());
        assert!(trusted_bin("git").unwrap().is_absolute());
        assert!(trusted_bin("no-such-program-here").is_err());
    }

    #[test]
    fn rebase_and_reset_in_a_real_repository() {
        let d = std::env::temp_dir().join(format!("issuewright-tools-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let sh = |dir: &Path, cmd: &str| {
            let ok = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success();
            assert!(ok, "{cmd}");
        };
        std::fs::create_dir_all(d.join("origin")).unwrap();
        let id = "-c user.name=t -c user.email=t@t -c commit.gpgsign=false";
        sh(
            &d.join("origin"),
            &format!("git init -q -b master && echo a > a && git add a && git {id} commit -qm a"),
        );
        sh(&d, "git clone -q origin slot");
        let slot = d.join("slot");
        sh(
            &slot,
            &format!("git switch -qc feat/1-x && echo b > b && git add b && git {id} commit -qm b"),
        );
        sh(
            &d.join("origin"),
            &format!(
                "mkdir -p .claude/skills && echo s > .claude/skills/x && git add . && git {id} commit -qm s"
            ),
        );
        // The test's git has no identity or signing config of the user's.
        let git = Git {
            bin: trusted_bin("git").unwrap(),
            dir: slot.clone(),
            home: d.clone(),
            ssh_auth_sock: None,
            gh_token: None,
        };
        std::fs::write(
            d.join(".gitconfig"),
            "[user]\nname = t\nemail = t@t\n[commit]\ngpgsign = false\n",
        )
        .unwrap();
        rebase(&git).unwrap();
        assert!(slot.join(".claude/skills/x").exists());
        assert!(slot.join("b").exists());
        // A half-changed worktree is cleaned up, committed work stays.
        std::fs::write(slot.join("a"), "changed").unwrap();
        std::fs::write(slot.join("new"), "untracked").unwrap();
        assert!(rebase(&git).is_err());
        reset(&git).unwrap();
        assert_eq!(std::fs::read_to_string(slot.join("a")).unwrap(), "a\n");
        assert!(!slot.join("new").exists());
        assert!(slot.join("b").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
