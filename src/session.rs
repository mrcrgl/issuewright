//! What a spawned session gets: its settings file (sandbox, deny rules,
//! notification hook), its environment, its prompt, and its command line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::{Config, RepoConfig, expand_home, home_dir};
use crate::model::{IssueRef, Snapshot};
use crate::slots::Slot;
use crate::state::{Kind, Mode, ReviewedPr};

pub const DEFAULT_PROMPT: &str = include_str!("prompt.md");
pub const DEFAULT_REVIEW_PROMPT: &str = include_str!("review_prompt.md");
pub const DEFAULT_PR_REVIEW_PROMPT: &str = include_str!("pr_review_prompt.md");
pub const DEFAULT_EPIC_CHECK_PROMPT: &str = include_str!("epic_check_prompt.md");
pub const CI_FAILED_PROMPT: &str = include_str!("ci_failed_prompt.md");
pub const REBASE_PROMPT: &str = include_str!("rebase_prompt.md");
pub const ANSWERED_PROMPT: &str = include_str!("answered_prompt.md");

/// How a session's conversation starts.
pub enum Start<'a> {
    /// A new conversation with this id (`--session-id`).
    New(&'a str),
    /// An earlier conversation, resumed (`--resume`).
    Resume(&'a str),
}

/// A fresh session id: a random (version 4) UUID.
pub fn new_session_id() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("reading /dev/urandom")?;
    Ok(uuid_v4(b))
}

/// Formats 16 random bytes as a version 4 UUID.
fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Where Claude Code keeps the transcripts of sessions started in `dir`:
/// `~/.claude/projects/<dir, every character but letters and digits as ->`.
pub fn transcript_dir(dir: &Path) -> PathBuf {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"));
    let name: String = dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config.join("projects").join(name)
}

/// When someone last gave the session in slot directory `dir` input: the
/// newest `user` entry in its newest transcript. That includes `!` shell
/// commands and messages from the Claude app, which issuewright's
/// `UserPromptSubmit` hook doesn't see or older sessions don't have.
pub fn last_input(dir: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    use std::io::{Read, Seek, SeekFrom};
    let newest = std::fs::read_dir(transcript_dir(dir))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max()?
        .1;
    // The end is enough, and a transcript can grow to many megabytes.
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(newest).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    last_input_in(&String::from_utf8_lossy(&buf))
}

/// The newest `user` entry's time in transcript lines; a line cut off at the
/// start of the tail doesn't parse and is skipped.
fn last_input_in(lines: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    lines.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        if v.get("type")?.as_str()? != "user" {
            return None;
        }
        v.get("timestamp")?.as_str()?.parse().ok()
    })
}

/// Copies session `id`'s transcript from the slot directory `from` to `to`,
/// so `claude --resume` finds it there. Whether there was one to copy.
pub fn move_transcript(id: &str, from: &Path, to: &Path) -> Result<bool> {
    let file = format!("{id}.jsonl");
    let src = transcript_dir(from).join(&file);
    if !src.is_file() {
        return Ok(false);
    }
    let dst = transcript_dir(to);
    std::fs::create_dir_all(&dst)?;
    std::fs::copy(&src, dst.join(&file))
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(true)
}

/// Variables of the message that resumes a session after CI failed.
pub struct CiFailedVars<'a> {
    pub repo: &'a str,
    pub mode: Mode,
    pub number: u64,
    pub pr: &'a IssueRef,
    pub head_sha: &'a str,
    pub failed: &'a [String],
    pub dir: &'a Path,
    pub branch: &'a str,
    /// The slot it ran in before, when it now runs in another one.
    pub moved_from: Option<&'a Path>,
}

pub fn render_ci_failed_prompt(v: &CiFailedVars) -> String {
    let failed = if v.failed.is_empty() {
        "- (GitHub names none; the rollup reports a failure)".to_string()
    } else {
        v.failed
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    CI_FAILED_PROMPT
        .replace("{mode}", &v.mode.to_string())
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{pr_number}", &v.pr.number.to_string())
        .replace("{pr}", &v.pr.to_string())
        .replace("{head_sha}", v.head_sha)
        .replace("{failed}", &failed)
        .replace("{dir}", &v.dir.to_string_lossy())
        .replace("{branch}", v.branch)
        .replace("{moved_note}", &moved_note(v.moved_from))
}

/// Variables of the message that resumes a session to rebase its approved
/// PR, which conflicts with the default branch.
pub struct RebaseVars<'a> {
    pub repo: &'a str,
    pub mode: Mode,
    pub number: u64,
    pub pr: &'a IssueRef,
    pub head_sha: &'a str,
    pub default_branch: &'a str,
    pub dir: &'a Path,
    pub branch: &'a str,
    pub moved_from: Option<&'a Path>,
}

pub fn render_rebase_prompt(v: &RebaseVars) -> String {
    REBASE_PROMPT
        .replace("{mode}", &v.mode.to_string())
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{pr}", &v.pr.to_string())
        .replace("{head_sha}", v.head_sha)
        .replace("{default_branch}", v.default_branch)
        .replace("{dir}", &v.dir.to_string_lossy())
        .replace("{branch}", v.branch)
        .replace("{moved_note}", &moved_note(v.moved_from))
}

/// Variables of the message that resumes a session whose question was
/// moved to GitHub, once the issue is released again.
pub struct AnsweredVars<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub dir: &'a Path,
    pub branch: &'a str,
    pub moved_from: Option<&'a Path>,
}

pub fn render_answered_prompt(v: &AnsweredVars) -> String {
    ANSWERED_PROMPT
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{dir}", &v.dir.to_string_lossy())
        .replace("{branch}", v.branch)
        .replace("{moved_note}", &moved_note(v.moved_from))
}

/// Tells a resumed session that it runs in another slot than before.
fn moved_note(from: Option<&Path>) -> String {
    match from {
        Some(old) => format!(
            " This is another directory than before (`{}`), because that slot was busy: paths from earlier in this conversation point there, so use the ones here.",
            old.display()
        ),
        None => String::new(),
    }
}

/// Deny rules reviewer and epic-check sessions get on top of the usual ones:
/// they judge code and must not change it. A reviewer must not move the
/// branch away from the commit it approves.
const REVIEW_DENY: [&str; 2] = ["Bash(git push *)", "Bash(git commit *)"];

/// Variables for the prompt template.
pub struct PromptVars<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub title: &'a str,
    pub mode: Mode,
    pub branch: &'a str,
    pub continued: bool,
    pub epic: String,
    pub blockers: String,
}

pub fn render_prompt(template: &str, v: &PromptVars) -> String {
    let note = if v.continued {
        " (it already existed on the remote, so you continue from the work pushed earlier; read the issue comments for what happened since)"
    } else {
        ""
    };
    template
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{title}", v.title)
        .replace("{mode}", &v.mode.to_string())
        .replace("{branch_note}", note)
        .replace("{branch}", v.branch)
        .replace("{epic}", &v.epic)
        .replace("{blockers}", &v.blockers)
}

/// Variables for the reviewer prompt template.
pub struct ReviewVars<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub title: &'a str,
    pub branch: &'a str,
    pub review: &'a ReviewedPr,
    pub epic: String,
}

pub fn render_review_prompt(template: &str, v: &ReviewVars) -> String {
    template
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{title}", v.title)
        .replace("{branch}", v.branch)
        .replace("{pr_number}", &v.review.pr.number.to_string())
        .replace("{pr}", &v.review.pr.to_string())
        .replace("{head_sha}", &v.review.head_sha)
        .replace("{epic}", &v.epic)
}

/// Variables for the prompt of a PR reviewed on its own, without an issue it
/// closes.
pub struct PrReviewVars<'a> {
    pub repo: &'a str,
    pub title: &'a str,
    pub branch: &'a str,
    pub review: &'a ReviewedPr,
}

pub fn render_pr_review_prompt(template: &str, v: &PrReviewVars) -> String {
    template
        .replace("{repo}", v.repo)
        .replace("{title}", v.title)
        .replace("{branch}", v.branch)
        .replace("{pr_number}", &v.review.pr.number.to_string())
        .replace("{pr}", &v.review.pr.to_string())
        .replace("{head_sha}", &v.review.head_sha)
}

/// Variables for the epic-check prompt template.
pub struct EpicCheckVars<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub title: &'a str,
    pub branch: &'a str,
    pub default_branch: &'a str,
}

pub fn render_epic_check_prompt(template: &str, v: &EpicCheckVars) -> String {
    template
        .replace("{repo}", v.repo)
        .replace("{number}", &v.number.to_string())
        .replace("{title}", v.title)
        .replace("{default_branch}", v.default_branch)
        .replace("{branch}", v.branch)
}

/// The prompt template for a session of `kind`: the configured file, or the
/// built-in one.
pub fn load_template(cfg: &Config, kind: Kind) -> Result<String> {
    let (file, builtin) = match kind {
        Kind::Implement => (&cfg.prompt_template, DEFAULT_PROMPT),
        Kind::Review => (&cfg.review_prompt_template, DEFAULT_REVIEW_PROMPT),
        Kind::EpicCheck => (&cfg.epic_check_prompt_template, DEFAULT_EPIC_CHECK_PROMPT),
    };
    read_template(file.as_deref(), builtin)
}

/// The prompt template of a PR reviewed on its own.
pub fn load_pr_review_template(cfg: &Config) -> Result<String> {
    read_template(
        cfg.pr_review_prompt_template.as_deref(),
        DEFAULT_PR_REVIEW_PROMPT,
    )
}

fn read_template(file: Option<&std::path::Path>, builtin: &str) -> Result<String> {
    match file {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("reading prompt template {}", p.display())),
        None => Ok(builtin.to_string()),
    }
}

/// `--model` and `--effort` for a session of `kind`.
pub fn model_effort(repo: &RepoConfig, kind: Kind) -> (Option<&str>, Option<&str>) {
    match kind {
        Kind::Implement | Kind::EpicCheck => (repo.model.as_deref(), repo.effort.as_deref()),
        Kind::Review => (
            repo.review_model.as_deref().or(repo.model.as_deref()),
            repo.review_effort.as_deref().or(repo.effort.as_deref()),
        ),
    }
}

/// `{epic}`: the parent epic's reference and title, or "none".
pub fn epic_text(snap: &Snapshot, parent: Option<&IssueRef>) -> String {
    match parent {
        None => "none".into(),
        Some(p) => match snap.epics.get(&p.key()) {
            Some(e) => format!("{p} ({})", e.status.title),
            None => p.to_string(),
        },
    }
}

/// `{blockers}`: each blocker with the PR that resolved it.
pub fn blockers_text(snap: &Snapshot, blockers: &[IssueRef]) -> String {
    if blockers.is_empty() {
        return "none".into();
    }
    blockers
        .iter()
        .map(|b| match snap.status(b).and_then(|s| s.merged_pr.clone()) {
            Some(pr) if pr.key() != b.key() => format!("{b} (resolved by PR {pr})"),
            Some(_) => format!("{b} (merged pull request)"),
            None => format!("{b} (closed)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn abs(p: &str) -> String {
    expand_home(Path::new(p)).to_string_lossy().into_owned()
}

/// `Read(...)` deny rules for the file tools. Home paths use `~/`, absolute
/// paths the `//` prefix.
fn read_rules(path: &str) -> Vec<String> {
    let p = if let Some(rest) = path.strip_prefix("~/") {
        format!("~/{rest}")
    } else if let Some(rest) = path.strip_prefix('/') {
        format!("//{rest}")
    } else {
        path.to_string()
    };
    let p = p.trim_end_matches('/').to_string();
    vec![format!("Read({p})"), format!("Read({p}/**)")]
}

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"))
}

/// The settings passed with `--settings`.
/// Merges `over` into `base`: tables key by key, everything else replaces.
pub fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn read_settings_file(p: &Path) -> Result<Value> {
    let s = std::fs::read_to_string(p)
        .with_context(|| format!("reading settings file {}", p.display()))?;
    serde_json::from_str(&s).with_context(|| format!("parsing settings file {}", p.display()))
}

/// The `--permission-mode` of a session: the repo's, else the session-wide one.
pub fn permission_mode<'a>(cfg: &'a Config, repo: &'a RepoConfig) -> Option<&'a str> {
    repo.permission_mode
        .as_deref()
        .or(cfg.session.permission_mode.as_deref())
}

/// The commands a session's hooks run.
pub struct Hooks<'a> {
    /// `Notification`: the session waits for input.
    pub notify: &'a str,
    /// `Stop`: after each turn; exits the session once it is finished.
    pub stop: &'a str,
    /// `UserPromptSubmit`: the session got an answer.
    pub prompt: &'a str,
    /// issuewright's binary, for `issuewright session …` (see [`crate::tools`]).
    pub issuewright: &'a str,
}

/// The settings passed with `--settings`: what issuewright generates from the
/// config, then `session.settings`, `session.settings_file`, the repo's
/// `settings` and `settings_file`, each merged over the previous.
pub fn settings(
    cfg: &Config,
    repo: &RepoConfig,
    slot: &Slot,
    hooks: &Hooks,
    kind: Kind,
) -> Result<Value> {
    let mut v = generated_settings(cfg, repo, slot, hooks, kind);
    let s = &cfg.session;
    if let Some(o) = &s.settings {
        deep_merge(&mut v, o);
    }
    if let Some(p) = &s.settings_file {
        deep_merge(&mut v, &read_settings_file(p)?);
    }
    if let Some(o) = &repo.settings {
        deep_merge(&mut v, o);
    }
    if let Some(p) = &repo.settings_file {
        deep_merge(&mut v, &read_settings_file(p)?);
    }
    // After the merges, which replace lists rather than extend them.
    if kind == Kind::Implement {
        add_tools(&mut v, hooks.issuewright);
    }
    Ok(v)
}

/// The rule that matches `issuewright session …` as the session must write it.
pub fn tools_rule(issuewright: &str) -> String {
    format!("{issuewright} session *")
}

/// Lets the session run `issuewright session …` outside the sandbox and without
/// the classifier; see [`crate::tools`].
fn add_tools(v: &mut Value, issuewright: &str) {
    let rule = tools_rule(issuewright);
    let push = |list: &mut Value, item: String| match list {
        Value::Array(a) => {
            if !a.contains(&Value::String(item.clone())) {
                a.push(Value::String(item));
            }
        }
        other => *other = json!([item]),
    };
    push(&mut v["permissions"]["allow"], format!("Bash({rule})"));
    if v["sandbox"].is_object() {
        push(&mut v["sandbox"]["excludedCommands"], rule);
    }
}

fn generated_settings(
    cfg: &Config,
    repo: &RepoConfig,
    slot: &Slot,
    hooks: &Hooks,
    kind: Kind,
) -> Value {
    let s = &cfg.session;
    let mut deny_read = s.effective_deny_read();
    // The review app's key would let a session approve its own PR.
    if let Some(k) = cfg.review_app.as_ref().and_then(|a| a.private_key.as_ref()) {
        deny_read.push(k.to_string_lossy().into_owned());
    }
    // The other sessions' keys for `issuewright session …`.
    if let Ok(d) = crate::tools::records_dir() {
        deny_read.push(d.to_string_lossy().into_owned());
    }
    let mut deny: Vec<String> = deny_read.iter().flat_map(|p| read_rules(p)).collect();
    deny.extend(s.effective_deny(repo));
    if matches!(kind, Kind::Review | Kind::EpicCheck) {
        deny.extend(REVIEW_DENY.map(String::from));
    }

    let mut allow_write = vec![
        slot.git_common_dir().to_string_lossy().into_owned(),
        slot.tmp.to_string_lossy().into_owned(),
        cargo_home().to_string_lossy().into_owned(),
    ];
    allow_write.extend(s.allow_write.iter().map(|p| abs(p)));
    // Programs that run outside the sandbox: issuewright itself (the hooks and
    // `issuewright session …`) and whatever else `cargo install` put in the shared
    // CARGO_HOME, which sessions may write otherwise.
    let mut deny_write = vec![cargo_home().join("bin").to_string_lossy().into_owned()];
    if let Some(d) = Path::new(hooks.issuewright).parent() {
        let d = d.to_string_lossy().into_owned();
        if !deny_write.contains(&d) {
            deny_write.push(d);
        }
    }

    let mut v = json!({
        "permissions": { "allow": s.effective_allow(repo), "deny": deny },
        "hooks": {
            "Notification": [ { "hooks": [ { "type": "command", "command": hooks.notify } ] } ],
            "Stop": [ { "hooks": [ { "type": "command", "command": hooks.stop, "timeout": 60 } ] } ],
            "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": hooks.prompt, "timeout": 10 } ] } ]
        }
    });
    if !s.disable_sandbox {
        v["sandbox"] = json!({
            "enabled": true,
            "failIfUnavailable": true,
            "allowUnsandboxedCommands": false,
            "autoAllowBashIfSandboxed": true,
            "filesystem": {
                "denyRead": deny_read.iter().map(|p| abs(p)).collect::<Vec<_>>(),
                "allowWrite": allow_write,
                "denyWrite": deny_write,
            },
            "network": { "allowedDomains": s.effective_allowed_domains(repo) },
        });
    }
    v
}

/// The session's whole environment; nothing else is inherited.
pub fn environment(
    cfg: &Config,
    repo: &RepoConfig,
    slot: &Slot,
    token: &str,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for k in &cfg.session.env_passthrough {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.clone(), v);
        }
    }
    if cfg.session.ssh_agent
        && let Ok(v) = std::env::var("SSH_AUTH_SOCK")
    {
        env.insert("SSH_AUTH_SOCK".into(), v);
    }
    env.insert("GH_TOKEN".into(), token.to_string());
    env.insert(
        "CARGO_HOME".into(),
        cargo_home().to_string_lossy().into_owned(),
    );
    env.insert(
        "CARGO_TARGET_DIR".into(),
        slot.target_dir().to_string_lossy().into_owned(),
    );
    env.insert("TMPDIR".into(), slot.tmp.to_string_lossy().into_owned());
    // gh would read the user's hosts.yml (other accounts' tokens), which the
    // sandbox blocks; an empty config dir makes it use GH_TOKEN alone.
    env.insert(
        "GH_CONFIG_DIR".into(),
        slot.tmp.join("gh-config").to_string_lossy().into_owned(),
    );
    env.insert("CLAUDE_CODE_SUBAGENT_MODEL".into(), "sonnet".into());
    for (k, v) in cfg.env.iter().chain(&repo.env) {
        env.insert(k.clone(), v.clone());
    }
    env
}

pub fn notify_command(cfg: &Config, issue: &IssueRef, name: &str) -> String {
    cfg.notify_command
        .replace("{repo}", &issue.repo)
        .replace("{number}", &issue.number.to_string())
        .replace("{name}", name)
}

/// `env -i K=V ... claude ...` for an interactive session.
pub fn interactive_argv(
    claude: &Path,
    env: &BTreeMap<String, String>,
    (model, effort): (Option<&str>, Option<&str>),
    (name, start): (&str, &Start),
    settings_path: &Path,
    (prompt, system): (&str, Option<&str>),
    permission_mode: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["env".to_string(), "-i".to_string()];
    argv.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    argv.push(claude.to_string_lossy().into_owned());
    argv.push(format!("--remote-control={name}"));
    argv.extend(["--name".into(), name.to_string()]);
    argv.extend([
        "--settings".into(),
        settings_path.to_string_lossy().into_owned(),
    ]);
    match start {
        Start::New(id) => argv.extend(["--session-id".into(), id.to_string()]),
        Start::Resume(id) => argv.extend(["--resume".into(), id.to_string()]),
    }
    if let Some(m) = model {
        argv.extend(["--model".into(), m.to_string()]);
    }
    if let Some(e) = effort {
        argv.extend(["--effort".into(), e.to_string()]);
    }
    if let Some(p) = permission_mode {
        argv.extend(["--permission-mode".into(), p.to_string()]);
    }
    if let Some(s) = system {
        argv.extend(["--append-system-prompt".into(), s.to_string()]);
    }
    argv.push("--".into());
    argv.push(prompt.to_string());
    argv
}

#[cfg(test)]
mod tests {
    #[test]
    fn answered_prompt_fills_every_placeholder() {
        let p = render_answered_prompt(&AnsweredVars {
            repo: "acme/app",
            number: 7,
            dir: Path::new("/slots/app-2"),
            branch: "feat/7-t",
            moved_from: Some(Path::new("/slots/app-1")),
        });
        assert!(p.contains("acme/app#7") && p.contains("`/slots/app-2`") && p.contains("feat/7-t"));
        assert!(p.contains("/slots/app-1"));
        for v in ["{repo}", "{number}", "{dir}", "{branch}", "{moved_note}"] {
            assert!(!p.contains(v), "{v} left in the prompt");
        }
    }

    use super::*;

    const H: Hooks = Hooks {
        notify: "n",
        stop: "s",
        prompt: "p",
        issuewright: "/bin/issuewright",
    };

    #[test]
    fn read_rule_prefixes() {
        assert_eq!(read_rules("~/.kube"), ["Read(~/.kube)", "Read(~/.kube/**)"]);
        assert_eq!(
            read_rules("/etc/secret/"),
            ["Read(//etc/secret)", "Read(//etc/secret/**)"]
        );
    }

    #[test]
    fn prompt_renders_all_variables() {
        let v = PromptVars {
            repo: "acme/app",
            number: 7,
            title: "Fix it",
            mode: Mode::Interactive,
            branch: "fix/7-fix-it",
            continued: false,
            epic: "none".into(),
            blockers: "none".into(),
        };
        let p = render_prompt(DEFAULT_PROMPT, &v);
        assert!(p.contains("acme/app#7: Fix it"));
        assert!(p.contains("branch `fix/7-fix-it`."));
        assert!(
            !p.contains('{') || !p.contains("{branch}"),
            "unrendered variable"
        );
        for var in [
            "{repo}",
            "{number}",
            "{title}",
            "{mode}",
            "{branch}",
            "{epic}",
            "{blockers}",
            "{branch_note}",
        ] {
            assert!(!p.contains(var), "{var} not rendered");
        }
    }

    #[test]
    fn overrides_replace_builtins_and_merge_raw_settings() {
        let cfg: Config = toml::from_str(
            r#"
[session]
builtin_deny = false
builtin_deny_read = false
builtin_allow = false
allow = ["Bash(cargo *)"]
permission_mode = "acceptEdits"

[session.settings.sandbox]
allowUnsandboxedCommands = true
[session.settings.sandbox.network]
allowedDomains = ["*"]

[[repo]]
name = "acme/app"
allow = ["Bash(make *)"]
permission_mode = "auto"
[repo.settings.permissions]
deny = ["Bash(rm -rf *)"]
"#,
        )
        .unwrap();
        let repo = &cfg.repos[0];
        let slot = Slot::new(&cfg, repo, 1);
        let s = settings(&cfg, repo, &slot, &H, Kind::Implement).unwrap();
        // Built-ins dropped, then the repo's raw list replaces the generated one.
        assert_eq!(s["permissions"]["deny"], json!(["Bash(rm -rf *)"]));
        assert_eq!(
            s["permissions"]["allow"],
            json!([
                "Bash(cargo *)",
                "Bash(make *)",
                "Bash(/bin/issuewright session *)"
            ])
        );
        // The session keys stay unreadable without the built-in list too.
        assert_eq!(
            s["sandbox"]["filesystem"]["denyRead"],
            json!([crate::tools::records_dir().unwrap()])
        );
        // Raw tables merge key by key: the override wins, the rest stays.
        assert_eq!(s["sandbox"]["allowUnsandboxedCommands"], true);
        assert_eq!(s["sandbox"]["enabled"], true);
        assert_eq!(s["sandbox"]["network"]["allowedDomains"], json!(["*"]));
        assert_eq!(permission_mode(&cfg, repo), Some("auto"));
        let other: Config = toml::from_str(
            "[session]\npermission_mode = \"acceptEdits\"\n[[repo]]\nname = \"a/b\"\n",
        )
        .unwrap();
        assert_eq!(
            permission_mode(&other, &other.repos[0]),
            Some("acceptEdits")
        );
    }

    #[test]
    fn settings_file_merges_last() {
        let dir = std::env::temp_dir().join(format!("issuewright-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.json");
        std::fs::write(&f, r#"{"sandbox": {"enabled": false}}"#).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "[[repo]]\nname = \"a/b\"\nsettings_file = {:?}\n",
            f.to_str().unwrap()
        ))
        .unwrap();
        let repo = &cfg.repos[0];
        let s = settings(&cfg, repo, &Slot::new(&cfg, repo, 1), &H, Kind::Implement).unwrap();
        assert_eq!(s["sandbox"]["enabled"], false);
        assert_eq!(s["sandbox"]["failIfUnavailable"], true);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settings_carry_sandbox_and_hook() {
        let cfg: Config = toml::from_str(
            "[session]\ndeny_read = [\"~/.config/cloud\"]\ndeny = [\"Bash(kubectl *)\"]\n[[repo]]\nname = \"acme/app\"\nallowed_domains = [\"example.org\"]\n",
        )
        .unwrap();
        let repo = &cfg.repos[0];
        let slot = Slot::new(&cfg, repo, 1);
        let s = settings(
            &cfg,
            repo,
            &slot,
            &Hooks {
                notify: "notify-me",
                stop: "stop-me",
                prompt: "prompt-me",
                issuewright: "/opt/issuewright",
            },
            Kind::Implement,
        )
        .unwrap();
        let deny: Vec<&str> = s["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(deny.contains(&"Read(~/.ssh/**)"));
        assert!(deny.contains(&"Read(~/.config/cloud/**)"));
        assert!(deny.contains(&"Bash(kubectl *)"));
        assert!(deny.contains(&"Bash(gh pr merge *)"));
        assert!(!deny.contains(&"Bash(git push *)"));
        // issuewright's own commands run outside the sandbox, without the classifier.
        assert_eq!(
            s["sandbox"]["excludedCommands"],
            json!(["/opt/issuewright session *"])
        );
        let allow = s["permissions"]["allow"].as_array().unwrap();
        assert!(
            allow
                .iter()
                .any(|a| a == "Bash(/opt/issuewright session *)")
        );
        assert!(allow.iter().any(|a| a == "Bash(gh run view *)"));
        // Nor can a session replace a program that runs outside the sandbox.
        assert_eq!(
            s["sandbox"]["filesystem"]["denyWrite"],
            json!([cargo_home().join("bin"), "/opt"])
        );
        // Their keys are out of every session's reach.
        let records = crate::tools::records_dir().unwrap();
        assert!(
            s["sandbox"]["filesystem"]["denyRead"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d.as_str() == records.to_str())
        );
        let with_app: Config = toml::from_str(
            "[review_app]\napp_id = 1\nprivate_key = \"/keys/app.pem\"\n[[repo]]\nname = \"acme/app\"\n",
        )
        .unwrap();
        let r = &with_app.repos[0];
        let sa = settings(
            &with_app,
            r,
            &Slot::new(&with_app, r, 1),
            &H,
            Kind::Implement,
        )
        .unwrap();
        assert!(
            sa["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "Read(//keys/app.pem)")
        );
        assert!(
            sa["sandbox"]["filesystem"]["denyRead"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "/keys/app.pem")
        );
        assert_eq!(s["sandbox"]["allowUnsandboxedCommands"], false);
        let domains = s["sandbox"]["network"]["allowedDomains"]
            .as_array()
            .unwrap();
        assert!(
            domains.iter().any(|d| d == "example.org") && domains.iter().any(|d| d == "github.com")
        );
        assert_eq!(
            s["hooks"]["Notification"][0]["hooks"][0]["command"],
            "notify-me"
        );
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "stop-me");
        assert_eq!(
            s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "prompt-me"
        );
    }

    #[test]
    fn last_input_is_the_newest_user_entry() {
        let lines = concat!(
            "ut\": \"cut off\"}\n",
            r#"{"type":"user","timestamp":"2026-11-23T10:05:46.000Z","message":{"content":"<bash-input>gh run view</bash-input>"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-23T10:05:52.000Z"}"#,
            "\n",
            r#"{"type":"system","timestamp":"2026-11-23T10:06:10.000Z"}"#,
            "\n",
        );
        assert_eq!(
            last_input_in(lines).map(|t| t.to_rfc3339()),
            Some("2026-11-23T10:05:46+00:00".into())
        );
        assert_eq!(last_input_in("not json\n"), None);
    }

    #[test]
    fn review_sessions_cannot_push_and_use_review_model() {
        let cfg: Config = toml::from_str(
            "[[repo]]\nname = \"acme/app\"\nmodel = \"sonnet\"\neffort = \"high\"\nreview_model = \"opus\"\n",
        )
        .unwrap();
        let repo = &cfg.repos[0];
        let s = settings(&cfg, repo, &Slot::new(&cfg, repo, 1), &H, Kind::Review).unwrap();
        let deny = s["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "Bash(git push *)"));
        assert!(deny.iter().any(|d| d == "Bash(gh pr merge *)"));
        assert_eq!(
            model_effort(repo, Kind::Review),
            (Some("opus"), Some("high"))
        );
        assert_eq!(
            model_effort(repo, Kind::Implement),
            (Some("sonnet"), Some("high"))
        );
    }

    #[test]
    fn review_prompt_renders_all_variables() {
        let review = ReviewedPr {
            pr: IssueRef::new("acme/app", 9),
            head_sha: "abc123".into(),
        };
        let p = render_review_prompt(
            DEFAULT_REVIEW_PROMPT,
            &ReviewVars {
                repo: "acme/app",
                number: 7,
                title: "Fix it",
                branch: "fix/7-fix-it",
                review: &review,
                epic: "none".into(),
            },
        );
        assert!(p.contains("pull request acme/app#9 for issue acme/app#7: Fix it"));
        assert!(p.contains("gh pr diff 9 --repo acme/app"));
        assert!(p.contains("`abc123`"));
        for var in [
            "{repo}",
            "{number}",
            "{title}",
            "{branch}",
            "{pr}",
            "{pr_number}",
            "{head_sha}",
            "{epic}",
        ] {
            assert!(!p.contains(var), "{var} not rendered");
        }
    }

    #[test]
    fn pr_review_prompt_renders_all_variables() {
        let review = ReviewedPr {
            pr: IssueRef::new("acme/app", 9),
            head_sha: "abc123".into(),
        };
        let p = render_pr_review_prompt(
            DEFAULT_PR_REVIEW_PROMPT,
            &PrReviewVars {
                repo: "acme/app",
                title: "Part one",
                branch: "feat/7-part-one",
                review: &review,
            },
        );
        assert!(p.contains("pull request acme/app#9: Part one"));
        assert!(p.contains("repos/acme/app/issues/9/labels"));
        assert!(!p.contains("gh issue edit"));
        for var in [
            "{repo}",
            "{title}",
            "{branch}",
            "{pr}",
            "{pr_number}",
            "{head_sha}",
        ] {
            assert!(!p.contains(var), "{var} not rendered");
        }
    }

    #[test]
    fn transcript_dir_encodes_the_slot_path() {
        let d = transcript_dir(Path::new("/mnt/work/slots/app/.base"));
        assert!(
            d.ends_with("projects/-mnt-work-slots-app--base"),
            "{}",
            d.display()
        );
    }

    #[test]
    fn ci_failed_prompt_renders_all_variables() {
        let pr = IssueRef::new("acme/app", 9);
        let failed = vec![
            "test: https://ci/1".to_string(),
            "lint: https://ci/2".to_string(),
        ];
        let v = CiFailedVars {
            mode: Mode::Interactive,
            repo: "acme/app",
            number: 7,
            pr: &pr,
            head_sha: "abc123",
            failed: &failed,
            dir: Path::new("/slots/app/2"),
            branch: "fix/7-x",
            moved_from: Some(Path::new("/slots/app/1")),
        };
        let p = render_ci_failed_prompt(&v);
        assert!(p.contains("CI failed on acme/app#9 at `abc123`"));
        assert!(p.contains("- test: https://ci/1\n- lint: https://ci/2"));
        assert!(p.contains("gh issue edit 7 --repo acme/app --add-label dispatch:ci"));
        assert!(p.contains("than before (`/slots/app/1`)"));
        assert!(p.contains("runs in interactive mode"));
        assert!(!p.contains('{'), "unrendered variable in {p}");
    }

    #[test]
    fn rebase_prompt_renders_all_variables() {
        let pr = IssueRef::new("acme/app", 9);
        let p = render_rebase_prompt(&RebaseVars {
            mode: Mode::Unattended,
            repo: "acme/app",
            number: 7,
            pr: &pr,
            head_sha: "abc123",
            default_branch: "main",
            dir: Path::new("/slots/app/1"),
            branch: "fix/7-x",
            moved_from: None,
        });
        assert!(p.contains("acme/app#9 was approved at `abc123`, but it conflicts with `main`"));
        assert!(p.contains("`git rebase origin/main` outside the sandbox"));
        assert!(p.contains("gh issue edit 7 --repo acme/app --add-label dispatch:ci"));
        assert!(!p.contains("another directory"));
        assert!(p.contains("runs in unattended mode"));
        assert!(!p.contains('{'), "unrendered variable in {p}");
    }

    #[test]
    fn tools_extend_the_configs_lists_and_skip_reviewers() {
        let cfg: Config = toml::from_str(
            "[session.settings.sandbox]\nexcludedCommands = [\"kubectl\"]\n[[repo]]\nname = \"acme/app\"\n",
        )
        .unwrap();
        let r = &cfg.repos[0];
        let slot = Slot::new(&cfg, r, 1);
        let s = settings(&cfg, r, &slot, &H, Kind::Implement).unwrap();
        assert_eq!(
            s["sandbox"]["excludedCommands"],
            json!(["kubectl", "/bin/issuewright session *"])
        );
        let rv = settings(&cfg, r, &slot, &H, Kind::Review).unwrap();
        assert_eq!(rv["sandbox"]["excludedCommands"], json!(["kubectl"]));
        assert!(
            !rv["permissions"]["allow"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a.as_str().unwrap().contains("session"))
        );
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        assert_eq!(uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
        let (a, b) = (new_session_id().unwrap(), new_session_id().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn argv_starts_or_resumes_a_conversation() {
        let env = BTreeMap::new();
        let argv = |s: &Start| {
            interactive_argv(
                Path::new("claude"),
                &env,
                (None, None),
                ("app#7", s),
                Path::new("s.json"),
                ("go", Some("tools")),
                None,
            )
        };
        let new = argv(&Start::New("id-1"));
        assert!(new.windows(2).any(|w| w == ["--session-id", "id-1"]));
        let resumed = argv(&Start::Resume("id-1"));
        assert!(resumed.windows(2).any(|w| w == ["--resume", "id-1"]));
        assert_eq!(resumed.last().map(String::as_str), Some("go"));
        assert!(
            new.windows(2)
                .any(|w| w == ["--append-system-prompt", "tools"])
        );
    }

    #[test]
    fn epic_check_prompt_renders_all_variables() {
        let p = render_epic_check_prompt(
            DEFAULT_EPIC_CHECK_PROMPT,
            &EpicCheckVars {
                repo: "acme/app",
                number: 40,
                title: "Epic",
                branch: "check/40",
                default_branch: "main",
            },
        );
        assert!(p.contains("epic acme/app#40: Epic"));
        assert!(p.contains("gh issue close 40 --repo acme/app"));
        for var in [
            "{repo}",
            "{number}",
            "{title}",
            "{branch}",
            "{default_branch}",
        ] {
            assert!(!p.contains(var), "{var} not rendered");
        }
    }
}
