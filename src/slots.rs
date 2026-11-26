//! The slot pool: persistent git worktrees per repository that keep their
//! `target/` across issues, so builds stay warm.
//!
//! Layout under `<home>/slots/<repo-short>/`:
//! - `.base/`  a `--no-checkout` clone that owns the worktrees
//! - `<i>/`    slot `i`, a worktree; its `target/` survives resets
//! - `tmp-<i>/` the slot's `TMPDIR`

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{Config, RepoConfig};

pub struct Slot {
    pub dir: PathBuf,
    pub tmp: PathBuf,
    pub base: PathBuf,
}

impl Slot {
    pub fn new(cfg: &Config, repo: &RepoConfig, index: usize) -> Self {
        let root = cfg.slots_dir().join(repo.short());
        Self {
            dir: root.join(index.to_string()),
            tmp: root.join(format!("tmp-{index}")),
            base: root.join(".base"),
        }
    }

    pub fn target_dir(&self) -> PathBuf {
        self.dir.join("target")
    }

    /// The base clone's git directory; the worktree writes objects and refs
    /// there, so the sandbox must allow it.
    pub fn git_common_dir(&self) -> PathBuf {
        self.base.join(".git")
    }
}

fn git(dir: &Path, token: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GH_TOKEN", token)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// How often a write of the shared `.git/config` is retried while another
/// git process holds `config.lock`, waiting 100 ms, then twice as long each
/// time (about six seconds in all). Sessions in the repository's other slots
/// run git against the same config, and git holds the lock for
/// milliseconds, so a claim that writes it at that moment would fail.
const CONFIG_LOCK_RETRIES: u32 = 6;

/// [`git`] for a command that writes the shared `.git/config`: retried while
/// the config is locked, clearing the locks that are known to be stale
/// before each retry.
fn git_writing_config(slot: &Slot, dir: &Path, token: &str, args: &[&str]) -> Result<String> {
    let mut wait = std::time::Duration::from_millis(100);
    let mut attempt = 0;
    loop {
        match git(dir, token, args) {
            Err(e) if attempt < CONFIG_LOCK_RETRIES && config_locked(&e) => {
                std::thread::sleep(wait);
                wait *= 2;
                attempt += 1;
                clear_stale_locks(slot)?;
            }
            res => return res,
        }
    }
}

/// Whether git failed because another process held `.git/config`'s lock.
fn config_locked(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("could not lock config file")
}

/// Creates the base clone and the slot's worktree when missing.
pub fn ensure(slot: &Slot, repo: &RepoConfig, token: &str) -> Result<()> {
    if !slot.base.join(".git").exists() {
        let parent = slot.base.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let url = repo.clone_url();
        git(parent, token, &["clone", "--no-checkout", &url, ".base"])?;
    }
    let base = &slot.base;
    clear_stale_locks(slot)?;
    set_config(
        slot,
        base,
        token,
        "commit.gpgsign",
        if repo.sign_commits { "true" } else { "false" },
    )?;
    // `git push` without arguments pushes the current branch to its namesake.
    set_config(slot, base, token, "push.default", "current")?;
    if !slot.dir.join(".git").exists() {
        git(base, token, &["fetch", "--prune", "origin"])?;
        let dir = slot.dir.to_string_lossy().into_owned();
        git(base, token, &["worktree", "prune"])?;
        git(
            base,
            token,
            &[
                "worktree",
                "add",
                "--detach",
                &dir,
                &format!("origin/{}", default_branch(base, token)?),
            ],
        )?;
    }
    std::fs::create_dir_all(&slot.tmp)?;
    Ok(())
}

pub fn default_branch(base: &Path, token: &str) -> Result<String> {
    let head = match git(
        base,
        token,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        Ok(h) => h,
        Err(_) => {
            git(base, token, &["remote", "set-head", "origin", "--auto"])?;
            git(
                base,
                token,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )?
        }
    };
    Ok(head.trim_start_matches("origin/").to_string())
}

/// What a reset left the slot on.
pub struct Reset {
    pub branch: String,
    /// The branch already existed on the remote (an issue released again);
    /// the slot continues from it instead of the default branch.
    pub continued: bool,
    pub default_branch: String,
}

/// Puts the slot on a clean `branch`, keeping `target/`.
pub fn reset(slot: &Slot, branch: &str, token: &str) -> Result<Reset> {
    let dir = &slot.dir;
    // `switch -C` and `set_upstream` write the shared `.git/config`.
    clear_stale_locks(slot)?;
    git(dir, token, &["fetch", "--prune", "origin"])?;
    let default = default_branch(&slot.base, token)?;
    let remote_branch = format!("origin/{branch}");
    let continued = git(
        dir,
        token,
        &["rev-parse", "--verify", "--quiet", &remote_branch],
    )
    .is_ok();
    let start = if continued {
        remote_branch
    } else {
        format!("origin/{default}")
    };
    git(dir, token, &["reset", "--hard"])?;
    git(dir, token, &["clean", "-fdx", "-e", "target/"])?;
    release_branch(slot, branch, token)?;
    // `--no-track`: `set_upstream` below sets the tracking, and writes the
    // shared config only when it changes.
    git_writing_config(
        slot,
        dir,
        token,
        &[
            "switch",
            "--discard-changes",
            "--no-track",
            "-C",
            branch,
            &start,
        ],
    )?;
    // Track the same-named remote branch, even before it exists. `switch -C`
    // from the default branch would track `origin/<default>`, and a session
    // can't fix that itself: the sandbox never lets it write `.git/config`, so
    // `git push -u` fails. With this, a plain `git push` works.
    set_upstream(slot, branch, token)?;
    git(dir, token, &["reset", "--hard"])?;
    git(dir, token, &["clean", "-fdx", "-e", "target/"])?;
    // Clear the slot's temp directory too.
    clear_dir(&slot.tmp)?;
    std::fs::create_dir_all(slot.tmp.join("gh-config"))?;
    Ok(Reset {
        branch: branch.to_string(),
        continued,
        default_branch: default,
    })
}

/// Removes `dir`, whatever a session left in it. Go's module cache is
/// read-only, so its directories are made writable first. What still can't
/// go, such as a socket directory a container's user owns, moves into
/// `.trash/` beside it, so the slot starts empty anyway; issuewright retries
/// removing it at every reset.
fn clear_dir(dir: &Path) -> Result<()> {
    let trash = dir
        .parent()
        .context("temp directory without a parent")?
        .join(".trash");
    if let Ok(entries) = std::fs::read_dir(&trash) {
        for e in entries.flatten() {
            remove_stubborn(&e.path()).ok();
        }
    }
    if !dir.exists() || remove_stubborn(dir).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(&trash)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = dir.file_name().context("temp directory without a name")?;
    let to = trash.join(format!("{}-{stamp}", name.to_string_lossy()));
    std::fs::rename(dir, &to).with_context(|| {
        format!(
            "clearing {}: neither removable nor movable to {}",
            dir.display(),
            to.display()
        )
    })
}

/// `remove_dir_all`, retried after making every directory under `p`
/// writable.
fn remove_stubborn(p: &Path) -> std::io::Result<()> {
    if std::fs::remove_dir_all(p).is_ok() {
        return Ok(());
    }
    make_writable(p);
    std::fs::remove_dir_all(p)
}

fn make_writable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mut perm = meta.permissions();
    perm.set_mode(perm.mode() | 0o700);
    std::fs::set_permissions(p, perm).ok();
    if let Ok(entries) = std::fs::read_dir(p) {
        for e in entries.flatten() {
            make_writable(&e.path());
        }
    }
}

/// Detaches every other worktree of the base clone that has `branch` checked
/// out. Git checks a branch out in one worktree at a time, and an idle slot
/// keeps the branch of its last session: an issue claimed again, or its
/// review, in another slot would otherwise fail with "already used by
/// worktree". The caller makes sure no running session holds `branch`.
fn release_branch(slot: &Slot, branch: &str, token: &str) -> Result<()> {
    let own = slot.dir.canonicalize().unwrap_or_else(|_| slot.dir.clone());
    let list = git(&slot.base, token, &["worktree", "list", "--porcelain"])?;
    let wanted = format!("refs/heads/{branch}");
    let mut path: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch ") == Some(wanted.as_str())
            && let Some(p) = path.take()
            && p.canonicalize().unwrap_or_else(|_| p.clone()) != own
        {
            git(&p, token, &["switch", "--detach"])?;
        }
    }
    Ok(())
}

/// The commit `origin/<branch>` points at, after a fetch; `None` when the
/// remote has no such branch.
pub fn remote_tip(slot: &Slot, branch: &str, token: &str) -> Result<Option<String>> {
    git(&slot.base, token, &["fetch", "--prune", "origin"])?;
    Ok(git(
        &slot.base,
        token,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
    )
    .ok())
}

/// The branch to work on: `base`, unless its remote tip is the head of an
/// already merged PR. Continuing such a branch would build on the code as it
/// was before a squash merge, and its push would be rejected; the work goes on
/// from the default branch on `<base>-2` (`-3`, ...) instead. `lookup(b)`
/// returns the merged PR heads of `b` and its remote tip.
pub fn unmerged_branch(
    base: &str,
    mut lookup: impl FnMut(&str) -> Result<(Vec<String>, Option<String>)>,
) -> Result<String> {
    for n in 1..=20 {
        let b = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        let (merged, tip) = lookup(&b)?;
        match tip {
            Some(t) if merged.contains(&t) => continue,
            _ => return Ok(b),
        }
    }
    bail!("{base}: twenty merged branches of that name; delete some")
}

/// Points `branch` at `origin/<branch>` for push and pull.
fn set_upstream(slot: &Slot, branch: &str, token: &str) -> Result<()> {
    let dir = &slot.dir;
    set_config(
        slot,
        dir,
        token,
        &format!("branch.{branch}.remote"),
        "origin",
    )?;
    set_config(
        slot,
        dir,
        token,
        &format!("branch.{branch}.merge"),
        &format!("refs/heads/{branch}"),
    )
}

/// Sets a git config value unless it already has it, so a claim writes the
/// shared `.git/config` only when something changes.
fn set_config(slot: &Slot, dir: &Path, token: &str, key: &str, value: &str) -> Result<()> {
    if git(dir, token, &["config", "--get", key]).is_ok_and(|v| v == value) {
        return Ok(());
    }
    git_writing_config(slot, dir, token, &["config", key, value])?;
    Ok(())
}

/// How old a lock in the shared git directory must be before issuewright takes it
/// for one a killed git process left behind. Git holds its locks for
/// milliseconds; a fetch holds the ref locks for seconds.
const STALE_LOCK: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Removes the git lock files that would make the slot's next git command
/// fail with "File exists", and returns them relative to the base clone's
/// git directory:
/// - the empty, read-only `config.lock` a session's sandbox leaves when the
///   session tries to write `.git/config` (not git's own: that one is
///   writable and gone once git exits);
/// - any lock in the slot's own worktree directory (`worktrees/<i>/`), such as
///   the `HEAD.lock` of a git command killed with its session. Only this slot
///   uses that directory, and issuewright prepares a slot only while it has no
///   session;
/// - locks elsewhere in the shared directory (`HEAD`, `logs/`, `refs/`,
///   `packed-refs`, `config`) older than [`STALE_LOCK`], since other slots'
///   sessions may hold a fresh one.
pub fn clear_stale_locks(slot: &Slot) -> Result<Vec<PathBuf>> {
    let common = slot.git_common_dir();
    let mut found = Vec::new();
    let config = common.join("config.lock");
    if let Ok(m) = std::fs::metadata(&config)
        && m.is_file()
        && m.len() == 0
        && m.permissions().readonly()
    {
        found.push(config);
    }
    if let Some(own) = worktree_git_dir(slot) {
        locks_in(&own, false, &mut found);
        locks_in(&own.join("logs"), false, &mut found);
    }
    let mut shared = Vec::new();
    locks_in(&common, false, &mut shared);
    locks_in(&common.join("logs"), false, &mut shared);
    locks_in(&common.join("refs"), true, &mut shared);
    let now = std::time::SystemTime::now();
    found.extend(shared.into_iter().filter(|p| {
        p.file_name().is_some_and(|n| n != "gc.pid.lock")
            && std::fs::metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > STALE_LOCK)
    }));
    found.sort();
    found.dedup();
    let mut removed = Vec::new();
    for p in found {
        match std::fs::remove_file(&p) {
            Ok(()) => removed.push(p.strip_prefix(&common).unwrap_or(&p).to_path_buf()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }
    Ok(removed)
}

/// The slot's own directory in the base clone's git directory, from the
/// worktree's `.git` file (`gitdir: <path>`).
fn worktree_git_dir(slot: &Slot) -> Option<PathBuf> {
    let link = std::fs::read_to_string(slot.dir.join(".git")).ok()?;
    let dir = PathBuf::from(link.strip_prefix("gitdir:")?.trim());
    Some(if dir.is_absolute() {
        dir
    } else {
        slot.dir.join(dir)
    })
}

/// Collects the `*.lock` files in `dir`, descending when `deep`.
fn locks_in(dir: &Path, deep: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let p = e.path();
        if t.is_dir() {
            if deep {
                locks_in(&p, true, out);
            }
        } else if t.is_file() && p.extension().is_some_and(|x| x == "lock") {
            out.push(p);
        }
    }
}

/// Size of a directory in bytes (`du`), zero when missing.
pub fn dir_size(p: &Path) -> Result<u64> {
    if !p.exists() {
        return Ok(0);
    }
    let out = Command::new("du").arg("-sb").arg(p).output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0))
}

/// Removes the slot's `target/` when it exceeds the configured size. Returns
/// the size removed, if any.
pub fn enforce_disk_cap(slot: &Slot, repo: &RepoConfig) -> Result<Option<u64>> {
    let Some(max_gb) = repo.target_dir_max_gb else {
        return Ok(None);
    };
    let target = slot.target_dir();
    let size = dir_size(&target)?;
    if size > max_gb * 1024 * 1024 * 1024 {
        std::fs::remove_dir_all(&target)
            .with_context(|| format!("removing {}", target.display()))?;
        return Ok(Some(size));
    }
    Ok(None)
}

/// `feat`, `fix`, `docs` or `chore`, from the issue's `type:*` label.
pub fn branch_kind(labels: &[String]) -> &'static str {
    let has = |l: &str| labels.iter().any(|x| x.eq_ignore_ascii_case(l));
    if has("type:bug") {
        "fix"
    } else if has("type:docs") {
        "docs"
    } else if has("type:chore") || has("type:spike") {
        "chore"
    } else {
        "feat"
    }
}

/// A short branch-safe slug of the title, without a leading `[Task]`-style tag.
pub fn slug(title: &str) -> String {
    let t = title.trim();
    let t = if t.starts_with('[') {
        t.split_once(']').map(|x| x.1).unwrap_or(t)
    } else {
        t
    };
    let mut out = String::new();
    for word in t
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if out.len() + word.len() + 1 > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word.to_ascii_lowercase());
    }
    if out.is_empty() { "issue".into() } else { out }
}

pub fn branch_name(template: &str, number: u64, title: &str, labels: &[String]) -> String {
    template
        .replace("{kind}", branch_kind(labels))
        .replace("{number}", &number.to_string())
        .replace("{slug}", &slug(title))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot on a fresh branch pushes with a plain `git push`, without `-u`,
    /// and never to the default branch.
    #[test]
    fn plain_push_creates_the_remote_branch() {
        let root = std::env::temp_dir().join(format!("issuewright-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let sh = |dir: &Path, args: &[&str]| git(dir, "", args).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        sh(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let remote = root.join("remote.git");
        sh(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );

        let cfg: Config = toml::from_str(&format!(
            "home = {:?}\n[[repo]]\nname = \"acme/app\"\nclone_url = {:?}\n",
            root.join("home").to_str().unwrap(),
            remote.to_str().unwrap()
        ))
        .unwrap();
        let repo = &cfg.repos[0];
        let slot = Slot::new(&cfg, repo, 1);
        ensure(&slot, repo, "").unwrap();
        let r = reset(&slot, "feat/1-x", "").unwrap();
        assert!(!r.continued);
        sh(
            &slot.dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "work",
            ],
        );
        sh(&slot.dir, &["push", "-q"]);
        let heads = sh(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        );
        assert_eq!(
            heads.lines().collect::<Vec<_>>(),
            ["refs/heads/feat/1-x", "refs/heads/main"]
        );
        assert_eq!(sh(&remote, &["rev-list", "--count", "main"]), "1");

        // Another slot's git holds the config lock for a moment: the claim
        // waits for it instead of failing.
        let held = slot.git_common_dir().join("config.lock");
        std::fs::write(&held, "[core]\n").unwrap();
        let release = {
            let held = held.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::fs::remove_file(held).unwrap();
            })
        };
        let r = reset(&slot, "feat/2-y", "").unwrap();
        release.join().unwrap();
        assert!(!r.continued);
        assert_eq!(
            sh(&slot.dir, &["config", "branch.feat/2-y.merge"]),
            "refs/heads/feat/2-y"
        );

        // A sandbox placeholder lock doesn't block the next claim.
        let lock = slot.git_common_dir().join("config.lock");
        std::fs::write(&lock, "").unwrap();
        let mut perm = std::fs::metadata(&lock).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&lock, perm).unwrap();

        // Released again: the slot continues from the pushed branch.
        let r = reset(&slot, "feat/1-x", "").unwrap();
        assert!(r.continued);
        assert_eq!(sh(&slot.dir, &["rev-list", "--count", "HEAD"]), "2");
        assert!(!lock.exists());
        ensure(&slot, repo, "").unwrap();

        // Claimed again in another slot while slot 1 still has the branch.
        let other = Slot::new(&cfg, repo, 2);
        ensure(&other, repo, "").unwrap();
        let r = reset(&other, "feat/1-x", "").unwrap();
        assert!(r.continued);
        assert_eq!(sh(&other.dir, &["branch", "--show-current"]), "feat/1-x");
        assert_eq!(sh(&slot.dir, &["branch", "--show-current"]), "");
        // And back: slot 1 takes it again from the now idle slot 2.
        reset(&slot, "feat/1-x", "").unwrap();
        assert_eq!(sh(&slot.dir, &["branch", "--show-current"]), "feat/1-x");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Locks a killed git command left behind don't block the slot: its own
    /// worktree's go at once, shared ones once they are old.
    #[test]
    fn stale_locks_are_removed() {
        let root = std::env::temp_dir().join(format!("issuewright-locks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let slot = Slot {
            dir: root.join("1"),
            tmp: root.join("tmp-1"),
            base: root.join(".base"),
        };
        let common = slot.git_common_dir();
        let own = common.join("worktrees/1");
        for d in [
            &slot.dir,
            &own.join("logs"),
            &common.join("refs/heads/feat"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            slot.dir.join(".git"),
            format!("gitdir: {}\n", own.display()),
        )
        .unwrap();
        let touch = |p: &Path, age_secs: u64| {
            std::fs::write(p, "").unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        touch(&own.join("HEAD.lock"), 0);
        touch(&own.join("logs/HEAD.lock"), 0);
        touch(&common.join("HEAD.lock"), 3600);
        touch(&common.join("refs/heads/feat/x.lock"), 3600);
        touch(&common.join("refs/heads/fresh.lock"), 0);
        touch(&common.join("gc.pid.lock"), 3600);
        touch(&common.join("worktrees/2.lock"), 3600);

        let removed = clear_stale_locks(&slot).unwrap();
        let names: Vec<_> = removed.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "HEAD.lock",
                "refs/heads/feat/x.lock",
                "worktrees/1/HEAD.lock",
                "worktrees/1/logs/HEAD.lock"
            ]
        );
        assert!(common.join("refs/heads/fresh.lock").exists());
        assert!(common.join("gc.pid.lock").exists());
        assert!(clear_stale_locks(&slot).unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merged_branches_are_not_continued() {
        let remote = |b: &str| -> Result<(Vec<String>, Option<String>)> {
            Ok(match b {
                // Squash-merged as PR head, then nothing pushed since.
                "feat/1-x" => (vec!["a".into()], Some("a".into())),
                // Merged once, then new commits pushed: continue it.
                "feat/2-y" => (vec!["b".into()], Some("c".into())),
                // Merged and the branch deleted: start it anew.
                "feat/3-z" => (vec!["d".into()], None),
                _ => (vec![], None),
            })
        };
        assert_eq!(unmerged_branch("feat/1-x", remote).unwrap(), "feat/1-x-2");
        assert_eq!(unmerged_branch("feat/2-y", remote).unwrap(), "feat/2-y");
        assert_eq!(unmerged_branch("feat/3-z", remote).unwrap(), "feat/3-z");
        assert_eq!(unmerged_branch("feat/4-new", remote).unwrap(), "feat/4-new");
        assert!(unmerged_branch("feat/x", |_| Ok((vec!["a".into()], Some("a".into())))).is_err());
    }

    #[test]
    fn branch_names() {
        let labels = vec!["type:bug".to_string()];
        assert_eq!(
            branch_name(
                "{kind}/{number}-{slug}",
                42,
                "[Bug] Login fails with `é` in the name!",
                &labels
            ),
            "fix/42-login-fails-with-in-the-name"
        );
        assert_eq!(
            slug("[Task] issuewright: long-running dispatcher that starts one fresh session"),
            "issuewright-long-running-dispatcher-that"
        );
        assert_eq!(slug("!!!"), "issue");
    }

    #[test]
    fn clear_dir_removes_read_only_trees() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("issuewright-clear-{}", std::process::id()));
        let tmp = root.join("tmp-1");
        let module = tmp.join("gopath/pkg/mod/x@v1");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("a.go"), "package x").unwrap();
        for d in [&module, &module.parent().unwrap().to_path_buf()] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        assert!(std::fs::remove_dir_all(&tmp).is_err());
        clear_dir(&tmp).unwrap();
        assert!(!tmp.exists());
        assert!(!root.join(".trash").exists());
        clear_dir(&tmp).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }
}
