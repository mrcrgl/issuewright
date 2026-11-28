//! The TOML configuration file and its defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// When a blocking issue counts as done.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockerDone {
    /// Only when the blocker is closed as completed.
    Closed,
    /// When the blocker is closed as completed, or a pull request that closes it
    /// has been merged into its repository's default branch.
    #[default]
    MergedOrClosed,
}

/// How issuewright merges a PR its reviewer session approved.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    #[default]
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    /// The method's name in GitHub's API and rulesets.
    pub fn name(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default)]
    pub blocker_done: BlockerDone,
    #[serde(default)]
    pub max_started_epics: Option<usize>,
    /// Shell command run when a session waits for input. `{repo}`, `{number}`
    /// and `{name}` are replaced.
    #[serde(default = "default_notify_command")]
    pub notify_command: String,
    /// Prompt template file. The built-in template is used when unset.
    #[serde(default)]
    pub prompt_template: Option<PathBuf>,
    /// Prompt template of reviewer sessions (`auto_merge`). The built-in one is
    /// used when unset.
    #[serde(default)]
    pub review_prompt_template: Option<PathBuf>,
    /// Prompt template of reviewer sessions for a PR that closes no issue,
    /// labeled itself. The built-in one is used when unset.
    #[serde(default)]
    pub pr_review_prompt_template: Option<PathBuf>,
    /// Prompt template of epic-check sessions. The built-in one is used when
    /// unset.
    #[serde(default)]
    pub epic_check_prompt_template: Option<PathBuf>,
    /// When all sub-issues of an epic are closed, start a session that checks
    /// the epic's acceptance criteria and closes it. On by default.
    #[serde(default = "yes")]
    pub check_epics: bool,
    /// Close a session's tmux window this many seconds after its issue reached
    /// its end label (`dispatch:review`, or a reviewer's verdict), which frees
    /// the slot. Sessions normally exit on their own at that point (their
    /// `Stop` hook); this is the fallback. Unset: no fallback.
    #[serde(default)]
    pub close_done_after_secs: Option<u64>,
    /// Interactive mode: when a session's question has had no answer for this
    /// many seconds, issuewright tells the session to move it to GitHub (a
    /// comment and `dispatch:needs-input`) and stop, which frees the slot.
    /// The next `dispatch:ready` resumes that conversation. 0: wait forever.
    #[serde(default = "default_question_timeout")]
    pub question_timeout_secs: u64,
    /// Shell command run when an error has repeated for `alarm_after_polls`
    /// polls, and again once it stops. `{message}` is replaced (quotes
    /// removed); `ISSUEWRIGHT_ALARM` holds it verbatim.
    #[serde(default = "default_alarm_command")]
    pub alarm_command: String,
    /// How many consecutive polls an error must show up in before it raises
    /// an alarm.
    #[serde(default = "default_alarm_after_polls")]
    pub alarm_after_polls: u32,
    /// New sessions wait while `home` or one of `disk_paths` has less than
    /// this many GB free. 0 turns it off.
    #[serde(default = "default_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
    /// More file systems to check for `min_free_disk_gb`, besides `home`'s,
    /// such as `/` when `/tmp` or Docker live there.
    #[serde(default)]
    pub disk_paths: Vec<PathBuf>,
    /// New sessions wait while the five-minute load average is above this.
    /// Off when unset.
    #[serde(default)]
    pub max_load: Option<f64>,
    /// The GitHub account whose token (`gh auth token -u <user>`) issuewright and the
    /// sessions use. The active `gh` account is used when unset.
    #[serde(default)]
    pub gh_user: Option<String>,
    /// Root of issuewright's own files: slots, state and logs.
    #[serde(default = "default_home")]
    pub home: PathBuf,
    /// Name of the tmux session interactive mode runs in.
    #[serde(default = "default_tmux_session")]
    pub tmux_session: String,
    /// Branch name template. `{kind}`, `{number}` and `{slug}` are replaced.
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
    /// Extra environment for every session.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub session: SessionConfig,
    /// The GitHub App reviewer sessions act as (`auto_merge`). Without it,
    /// reviews are disabled.
    #[serde(default)]
    pub review_app: Option<ReviewAppConfig>,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
}

/// A GitHub App for reviewer sessions: their approvals then come from
/// `<app>[bot]`, which GitHub counts, instead of from the PR's author.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAppConfig {
    pub app_id: u64,
    /// PEM file of the app's private key. Sessions can never read it.
    #[serde(default)]
    pub private_key: Option<PathBuf>,
    /// Or a command printing the key, e.g. `pass show github/issuewright-app`.
    #[serde(default)]
    pub private_key_command: Option<String>,
}

/// Guardrails and environment of the spawned sessions, shared by all repos.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Variables passed through from issuewright's own environment. Everything else
    /// is dropped.
    #[serde(default = "default_env_passthrough")]
    pub env_passthrough: Vec<String>,
    /// Paths the session can't read, neither from Bash (sandbox) nor with the
    /// file tools (permission rules), in addition to the built-in list. `~` is
    /// the home directory.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Hosts Bash may reach through the sandbox proxy, in addition to the
    /// built-in list.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Extra paths Bash may write besides the slot and its temp directory.
    #[serde(default)]
    pub allow_write: Vec<String>,
    /// Permission deny rules, e.g. `Bash(kubectl *)`, in addition to the
    /// built-in ones (dispatching workflows, merging pull requests).
    #[serde(default)]
    pub deny: Vec<String>,
    /// Shell command printing the GitHub token the sessions get, e.g. a
    /// fine-grained token without Actions access. issuewright's own token
    /// (`gh_user`) is used when unset.
    #[serde(default)]
    pub gh_token_command: Option<String>,
    /// Pass `SSH_AUTH_SOCK` through. Off by default. With `signing_key`,
    /// sessions get issuewright's signing agent instead.
    #[serde(default)]
    pub ssh_agent: bool,
    /// A private SSH key without a passphrase that signs the sessions'
    /// commits, such as `~/.ssh/id_ed25519_signing`. issuewright loads it
    /// into an agent of its own; sessions sign through the agent and never
    /// read the key. Register it on GitHub as a signing key.
    #[serde(default)]
    pub signing_key: Option<PathBuf>,
    /// Turn the Bash sandbox off. Only for hosts without bubblewrap/socat;
    /// the credential paths then stay readable from Bash.
    #[serde(default)]
    pub disable_sandbox: bool,
    /// Keep the built-in deny rules (merging PRs, dispatching workflows,
    /// pushing to the default branch). `false` drops them; `deny` still applies.
    #[serde(default = "yes")]
    pub builtin_deny: bool,
    /// Keep the built-in credential paths (`~/.ssh`, `~/.kube`, ...).
    #[serde(default = "yes")]
    pub builtin_deny_read: bool,
    /// Keep the built-in network allowlist (GitHub, package registries).
    #[serde(default = "yes")]
    pub builtin_allowed_domains: bool,
    /// Keep the built-in allow rules: reading CI, opening PRs and issues,
    /// commenting, pushing a rebased branch. In auto mode they keep the
    /// classifier from refusing a session's routine steps.
    #[serde(default = "yes")]
    pub builtin_allow: bool,
    /// Permission allow rules, e.g. `Bash(cargo *)`. They add to the user's own
    /// `~/.claude/settings.json`, which sessions read as well.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Default `--permission-mode` for all repos (a repo's own wins).
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Raw Claude Code settings, deep-merged over what issuewright generates:
    /// tables merge key by key, anything else (lists included) replaces.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    /// A JSON settings file, merged after `settings`.
    #[serde(default)]
    pub settings_file: Option<PathBuf>,
}

fn yes() -> bool {
    true
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            env_passthrough: default_env_passthrough(),
            deny_read: Vec::new(),
            allowed_domains: Vec::new(),
            allow_write: Vec::new(),
            deny: Vec::new(),
            gh_token_command: None,
            ssh_agent: false,
            signing_key: None,
            disable_sandbox: false,
            builtin_deny: true,
            builtin_deny_read: true,
            builtin_allowed_domains: true,
            builtin_allow: true,
            allow: Vec::new(),
            permission_mode: None,
            settings: None,
            settings_file: None,
        }
    }
}

impl SessionConfig {
    pub fn effective_deny_read(&self) -> Vec<String> {
        merged(
            builtin(self.builtin_deny_read, default_deny_read),
            &self.deny_read,
        )
    }
    pub fn effective_deny(&self, repo: &RepoConfig) -> Vec<String> {
        merged(
            merged(builtin(self.builtin_deny, default_deny), &self.deny),
            &repo.deny,
        )
    }
    pub fn effective_allow(&self, repo: &RepoConfig) -> Vec<String> {
        merged(
            merged(builtin(self.builtin_allow, default_allow), &self.allow),
            &repo.allow,
        )
    }
    pub fn effective_allowed_domains(&self, repo: &RepoConfig) -> Vec<String> {
        merged(
            merged(
                builtin(self.builtin_allowed_domains, default_allowed_domains),
                &self.allowed_domains,
            ),
            &repo.allowed_domains,
        )
    }
}

fn builtin(keep: bool, f: fn() -> Vec<String>) -> Vec<String> {
    if keep { f() } else { Vec::new() }
}

fn merged(mut base: Vec<String>, extra: &[String]) -> Vec<String> {
    for e in extra {
        if !base.contains(e) {
            base.push(e.clone());
        }
    }
    base
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// `owner/name`.
    pub name: String,
    #[serde(default = "default_slots")]
    pub slots: usize,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Review and merge finished PRs: a separate reviewer session reviews the
    /// PR of each `dispatch:review` issue, and issuewright merges an approved PR
    /// (through GitHub's auto-merge while checks are pending). Off by default.
    #[serde(default)]
    pub auto_merge: bool,
    #[serde(default)]
    pub merge_method: MergeMethod,
    /// No new claims in this repository while this many of its issues wait
    /// for CI (`dispatch:ci`). CI fixes and reviews still start. Unlimited
    /// when unset.
    #[serde(default)]
    pub max_in_ci: Option<usize>,
    /// Model and effort of reviewer sessions; `model` and `effort` when unset.
    #[serde(default)]
    pub review_model: Option<String>,
    #[serde(default)]
    pub review_effort: Option<String>,
    /// Unattended mode only (not implemented yet).
    #[serde(default)]
    #[allow(dead_code)]
    pub max_budget_usd: Option<f64>,
    /// The slot's `target/` is removed before a claim once it exceeds this.
    #[serde(default)]
    pub target_dir_max_gb: Option<u64>,
    /// Clone URL. HTTPS on github.com by default, so pushing works with the
    /// token and without `~/.ssh`.
    #[serde(default)]
    pub clone_url: Option<String>,
    /// Sign the sessions' commits with `session.signing_key`. On by default
    /// when that is set; `false` leaves this repository's commits unsigned.
    #[serde(default)]
    pub sign_commits: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Raw Claude Code settings for this repo, merged after the session-wide
    /// `settings` and `settings_file`.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub settings_file: Option<PathBuf>,
}

impl RepoConfig {
    /// Whether this repository's sessions sign their commits: with a
    /// `session.signing_key`, unless `sign_commits = false`.
    pub fn signs(&self, cfg_has_key: bool) -> bool {
        cfg_has_key && self.sign_commits != Some(false)
    }

    pub fn owner_name(&self) -> (&str, &str) {
        self.name
            .split_once('/')
            .expect("validated in Config::load")
    }

    pub fn short(&self) -> &str {
        self.owner_name().1
    }

    pub fn clone_url(&self) -> String {
        self.clone_url
            .clone()
            .unwrap_or_else(|| format!("https://github.com/{}.git", self.name))
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.home = expand_home(&cfg.home);
        cfg.session.signing_key = cfg.session.signing_key.as_deref().map(expand_home);
        cfg.prompt_template = cfg.prompt_template.as_deref().map(expand_home);
        cfg.review_prompt_template = cfg.review_prompt_template.as_deref().map(expand_home);
        cfg.pr_review_prompt_template = cfg.pr_review_prompt_template.as_deref().map(expand_home);
        cfg.epic_check_prompt_template = cfg.epic_check_prompt_template.as_deref().map(expand_home);
        cfg.session.settings_file = cfg.session.settings_file.as_deref().map(expand_home);
        cfg.disk_paths = cfg.disk_paths.iter().map(|p| expand_home(p)).collect();
        if let Some(a) = &mut cfg.review_app {
            a.private_key = a.private_key.as_deref().map(expand_home);
        }
        for r in &mut cfg.repos {
            r.settings_file = r.settings_file.as_deref().map(expand_home);
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.repos.is_empty() {
            bail!("no [[repo]] configured");
        }
        if self.max_sessions == 0 {
            bail!("max_sessions must be at least 1");
        }
        let mut seen = std::collections::BTreeSet::new();
        for r in &self.repos {
            let Some((o, n)) = r.name.split_once('/') else {
                bail!("repo name {:?} is not owner/name", r.name);
            };
            if o.is_empty() || n.is_empty() || n.contains('/') {
                bail!("repo name {:?} is not owner/name", r.name);
            }
            if !seen.insert(r.name.to_lowercase()) {
                bail!("repo {} configured twice", r.name);
            }
            if r.slots == 0 {
                bail!("repo {} needs at least one slot", r.name);
            }
        }
        if let Some(a) = &self.review_app
            && a.private_key.is_some() == a.private_key_command.is_some()
        {
            bail!("review_app needs exactly one of private_key and private_key_command");
        }
        if !self.branch_template.contains("{number}") {
            bail!("branch_template must contain {{number}}");
        }
        Ok(())
    }

    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }

    pub fn slots_dir(&self) -> PathBuf {
        self.home.join("slots")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.home.join("state")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
}

/// The settings that differ between two versions of a config file, as key
/// paths (`max_sessions`, `repo owner/name: auto_merge`). Comments and
/// formatting don't count. `None` when either doesn't parse.
pub fn changed_keys(old: &str, new: &str) -> Option<Vec<String>> {
    let old: toml::Table = toml::from_str(old).ok()?;
    let new: toml::Table = toml::from_str(new).ok()?;
    let mut out = Vec::new();
    diff_tables("", &old, &new, &mut out);
    Some(out)
}

fn diff_tables(at: &str, old: &toml::Table, new: &toml::Table, out: &mut Vec<String>) {
    let keys: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for k in keys {
        let path = if at.is_empty() {
            k.clone()
        } else if at.ends_with(':') {
            format!("{at} {k}")
        } else {
            format!("{at}.{k}")
        };
        diff_values(&path, old.get(k), new.get(k), out);
    }
}

fn diff_values(
    at: &str,
    old: Option<&toml::Value>,
    new: Option<&toml::Value>,
    out: &mut Vec<String>,
) {
    use toml::Value;
    match (old, new) {
        (Some(Value::Table(a)), Some(Value::Table(b))) => diff_tables(at, a, b, out),
        // Arrays of named tables (`[[repo]]`): matched by name, not position.
        (Some(Value::Array(a)), Some(Value::Array(b)))
            if a.iter().chain(b).all(|v| named(v).is_some()) =>
        {
            let find = |xs: &Vec<Value>, n: &str| xs.iter().find(|v| named(v) == Some(n)).cloned();
            let names: std::collections::BTreeSet<&str> =
                a.iter().chain(b).filter_map(named).collect();
            for n in names {
                match (find(a, n), find(b, n)) {
                    (Some(Value::Table(x)), Some(Value::Table(y))) => {
                        diff_tables(&format!("{at} {n}:"), &x, &y, out)
                    }
                    (None, Some(_)) => out.push(format!("{at} {n} (added)")),
                    (Some(_), None) => out.push(format!("{at} {n} (removed)")),
                    _ => {}
                }
            }
        }
        (a, b) if a != b => out.push(at.to_string()),
        _ => {}
    }
}

fn named(v: &toml::Value) -> Option<&str> {
    v.as_table()?.get("name")?.as_str()
}

pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("issuewright/config.toml")
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~/` to the home directory.
pub fn expand_home(p: &Path) -> PathBuf {
    match p.to_str() {
        Some("~") => home_dir(),
        Some(s) if s.starts_with("~/") => home_dir().join(&s[2..]),
        _ => p.to_path_buf(),
    }
}

fn default_poll_interval() -> u64 {
    60
}
fn default_question_timeout() -> u64 {
    900
}
fn default_max_sessions() -> usize {
    3
}
fn default_slots() -> usize {
    1
}
fn default_alarm_command() -> String {
    "tmux display-message -d 15000 'issuewright: {message}'".into()
}
fn default_alarm_after_polls() -> u32 {
    3
}
fn default_min_free_disk_gb() -> u64 {
    10
}
fn default_notify_command() -> String {
    "tmux display-message 'issuewright: {name} waits for input'".into()
}
fn default_home() -> PathBuf {
    home_dir().join("issuewright")
}
fn default_tmux_session() -> String {
    "agents".into()
}
fn default_branch_template() -> String {
    "{kind}/{number}-{slug}".into()
}
fn default_env_passthrough() -> Vec<String> {
    [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "LC_ALL",
        "COLORTERM",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ]
    .map(String::from)
    .to_vec()
}
fn default_deny_read() -> Vec<String> {
    [
        "~/.ssh",
        "~/.kube",
        "~/.aws",
        "~/.azure",
        "~/.config/gcloud",
        "~/.docker/config.json",
        "~/.config/gh/hosts.yml",
        "~/.netrc",
        "~/.npmrc",
        "~/.cargo/credentials",
        "~/.cargo/credentials.toml",
        "~/.pypirc",
        "~/.gnupg",
        "~/.password-store",
    ]
    .map(String::from)
    .to_vec()
}
/// Steps that reach production through GitHub itself: a merge into the
/// default branch can deploy, and so can a dispatched workflow.
fn default_deny() -> Vec<String> {
    [
        "Bash(gh pr merge *)",
        "Bash(gh workflow run *)",
        "Bash(gh workflow enable *)",
        "Bash(gh run rerun *)",
        "Bash(gh api *dispatches*)",
        "Bash(gh api *merge*)",
        "Bash(git push * master*)",
        "Bash(git push * main*)",
        "Bash(git push * HEAD:master*)",
        "Bash(git push * HEAD:main*)",
    ]
    .map(String::from)
    .to_vec()
}

/// A session's routine GitHub and git steps. The deny rules still win: a
/// push to the default branch stays refused, and so does a merge.
fn default_allow() -> Vec<String> {
    [
        // CI: status, logs, artifacts.
        "Bash(gh pr checks *)",
        "Bash(gh run list *)",
        "Bash(gh run view *)",
        "Bash(gh run download *)",
        "Bash(gh api repos/*/actions/*)",
        "Bash(gh api --allow-escape-sequences repos/*/actions/*)",
        // Pull requests and issues of the session's own work.
        "Bash(gh pr view *)",
        "Bash(gh pr list *)",
        "Bash(gh pr diff *)",
        "Bash(gh pr create *)",
        "Bash(gh pr comment *)",
        "Bash(gh issue view *)",
        "Bash(gh issue list *)",
        "Bash(gh issue create *)",
        "Bash(gh issue comment *)",
        "Bash(gh issue edit *)",
        // After a rebase.
        "Bash(git push --force-with-lease)",
        "Bash(git push --force-with-lease *)",
    ]
    .map(String::from)
    .to_vec()
}

fn default_allowed_domains() -> Vec<String> {
    [
        "github.com",
        "api.github.com",
        "*.githubusercontent.com",
        "codeload.github.com",
        "crates.io",
        "index.crates.io",
        "static.crates.io",
        "static.rust-lang.org",
        "registry.npmjs.org",
        "proxy.golang.org",
        "sum.golang.org",
        "pypi.org",
        "files.pythonhosted.org",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_keys_name_settings_not_lines() {
        let old = r#"
max_sessions = 3
[session]
deny = ["Bash(helm *)"]
[[repo]]
name = "o/a"
slots = 2
[[repo]]
name = "o/b"
"#;
        let new = r#"
# a comment
max_sessions = 3
[session]
deny = ["Bash(helm *)", "Bash(psql *)"]
[[repo]]
name = "o/b"
auto_merge = true
[[repo]]
name = "o/a"
slots = 2
[[repo]]
name = "o/c"
"#;
        assert_eq!(
            changed_keys(old, new).unwrap(),
            ["repo o/b: auto_merge", "repo o/c (added)", "session.deny"]
        );
        assert_eq!(changed_keys(old, old).unwrap(), Vec::<String>::new());
        assert!(changed_keys(old, "x = ").is_none());
    }

    #[test]
    fn parses_example_config() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.blocker_done, BlockerDone::MergedOrClosed);
        assert_eq!(cfg.repos[0].short(), "backend");
        assert!(cfg.repos[0].auto_merge);
        assert_eq!(cfg.repos[0].merge_method, MergeMethod::Squash);
        assert_eq!(cfg.repos[0].max_in_ci, Some(6));
        assert_eq!(cfg.repos[1].max_in_ci, None);
        assert!(!cfg.repos[1].auto_merge);
        assert_eq!(cfg.close_done_after_secs, Some(600));
    }

    #[test]
    fn rejects_bad_repo_name() {
        let cfg: Config = toml::from_str("[[repo]]\nname = \"nope\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}
