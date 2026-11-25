//! GitHub through the `gh` CLI: the issue snapshot (GraphQL), labels and
//! comments.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::body::dependency_refs;
use crate::config::MergeMethod;
use crate::model::*;

/// How an approved PR was merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merged {
    /// Auto-merge is on: GitHub merges once the required checks pass.
    Auto,
    /// The PR could merge already and is merged.
    Now,
}

pub struct Gh {
    token: String,
    /// The `gh` binary; tests point it at a stub.
    bin: String,
}

impl Gh {
    /// Uses the token of `user`, or of the active `gh` account.
    pub fn new(user: Option<&str>) -> Result<Self> {
        let mut cmd = Command::new("gh");
        cmd.args(["auth", "token"]);
        if let Some(u) = user {
            cmd.args(["-u", u]);
        }
        let out = cmd.output().context("running gh auth token")?;
        if !out.status.success() {
            bail!(
                "gh auth token failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let token = String::from_utf8(out.stdout)?.trim().to_string();
        if token.is_empty() {
            bail!("gh auth token printed nothing");
        }
        Ok(Self::with_token(token))
    }

    /// Uses `token` as given, e.g. a session's own `GH_TOKEN`.
    pub fn with_token(token: String) -> Self {
        Self {
            token,
            bin: "gh".into(),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn gh(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("GH_TOKEN", &self.token)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        c
    }

    fn run(&self, mut cmd: Command, stdin: Option<&str>) -> Result<(bool, String, String)> {
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawning gh")?;
        if let Some(input) = stdin {
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// Runs a GraphQL query. Returns `data` and the `errors` array; partial
    /// results (some aliases unresolved) are not an error.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<(Value, Vec<Value>)> {
        let body = json!({ "query": query, "variables": variables }).to_string();
        let mut cmd = self.gh();
        cmd.args(["api", "graphql", "--input", "-"]);
        let (_, stdout, stderr) = self.run(cmd, Some(&body))?;
        let v: Value = serde_json::from_str(&stdout)
            .map_err(|e| anyhow!("gh api graphql: {e}; stderr: {}", stderr.trim()))?;
        let errors = v
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match v.get("data") {
            Some(d) if !d.is_null() => Ok((d.clone(), errors)),
            _ => bail!("gh api graphql returned no data: {}", Value::Array(errors)),
        }
    }

    /// Adds and removes labels on an issue or a pull request.
    pub fn edit_labels(&self, r: &IssueRef, add: &[&str], remove: &[&str]) -> Result<()> {
        if !add.is_empty() {
            let mut cmd = self.gh();
            cmd.args([
                "api",
                "-X",
                "POST",
                &format!("repos/{}/issues/{}/labels", r.repo, r.number),
                "--input",
                "-",
            ]);
            let input = json!({ "labels": add }).to_string();
            let (ok, _, err) = self.run(cmd, Some(&input))?;
            if !ok {
                bail!("adding {} to {r}: {}", add.join(", "), err.trim());
            }
        }
        for l in remove {
            self.remove_label(r, l)?;
        }
        Ok(())
    }

    /// Comments on an issue or a pull request.
    pub fn comment(&self, r: &IssueRef, body: &str) -> Result<()> {
        let mut cmd = self.gh();
        cmd.args([
            "api",
            "-X",
            "POST",
            &format!("repos/{}/issues/{}/comments", r.repo, r.number),
            "--input",
            "-",
        ]);
        let input = json!({ "body": body }).to_string();
        let (ok, _, err) = self.run(cmd, Some(&input))?;
        if !ok {
            bail!("commenting on {r}: {}", err.trim());
        }
        Ok(())
    }

    /// The open pull request in the issue's own repository that closes the
    /// issue and targets the default branch, if there is one.
    pub fn open_pr(&self, issue: &IssueRef) -> Result<Option<PullRequest>> {
        let (owner, name) = issue
            .repo
            .split_once('/')
            .context("repo is not owner/name")?;
        let query = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      closedByPullRequestsReferences(first: 10, includeClosedPrs: false) {
        nodes { number state isDraft headRefName headRefOid baseRefName
                repository { nameWithOwner } baseRepository { defaultBranchRef { name } } }
      }
    }
  }
}"#;
        let vars = json!({ "owner": owner, "name": name, "number": issue.number });
        let (data, errors) = self.graphql(query, vars)?;
        let issue_node = data
            .get("repository")
            .and_then(|r| r.get("issue"))
            .filter(|i| !i.is_null())
            .with_context(|| format!("{issue}: not found; errors: {errors:?}"))?;
        Ok(parse_open_pr(issue_node, &issue.repo))
    }

    /// Removes `label` from an issue or a pull request.
    pub fn remove_label(&self, r: &IssueRef, label: &str) -> Result<()> {
        let mut cmd = self.gh();
        cmd.args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{}/issues/{}/labels/{label}", r.repo, r.number),
        ]);
        let (ok, _, err) = self.run(cmd, None)?;
        if !ok && !err.contains("HTTP 404") {
            bail!("removing {label} from {r}: {}", err.trim());
        }
        Ok(())
    }

    /// State, labels and sub-issue counts of the given issues, in one query
    /// per 50. Issues that can't be read are missing from the result.
    pub fn issue_briefs(&self, issues: &[IssueRef]) -> Result<BTreeMap<(String, u64), IssueBrief>> {
        let mut out = BTreeMap::new();
        for chunk in issues.chunks(50) {
            let mut q = String::from("query {\n");
            for (i, r) in chunk.iter().enumerate() {
                let (owner, name) = r.repo.split_once('/').unwrap_or(("", &r.repo));
                q.push_str(&format!(
                    "  s{i}: repository(owner: {owner:?}, name: {name:?}) {{ issue(number: {n}) {{ {BRIEF_FIELDS} }} }}\n",
                    n = r.number
                ));
            }
            q.push('}');
            let (data, _) = self.graphql(&q, json!({}))?;
            for (i, r) in chunk.iter().enumerate() {
                if let Some(b) = data
                    .get(format!("s{i}"))
                    .and_then(|v| v.get("issue"))
                    .and_then(|v| parse_brief(r, v))
                {
                    out.insert(r.key(), b);
                }
            }
        }
        Ok(out)
    }

    /// Whether a pull request can merge into its base (`MERGEABLE`,
    /// `CONFLICTING`, or `UNKNOWN` while GitHub still computes it), and
    /// whether its auto-merge is on.
    pub fn mergeable(&self, pr: &IssueRef) -> Result<(String, bool)> {
        let (owner, name) = pr.repo.split_once('/').context("repo is not owner/name")?;
        let query = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) { mergeable autoMergeRequest { enabledAt } }
  }
}"#;
        let vars = json!({ "owner": owner, "name": name, "number": pr.number });
        let (data, errors) = self.graphql(query, vars)?;
        let p = &data["repository"]["pullRequest"];
        let state = str_at(p, &["mergeable"])
            .with_context(|| format!("{pr}: not found; errors: {errors:?}"))?;
        Ok((state.to_string(), !p["autoMergeRequest"].is_null()))
    }

    /// Turns auto-merge off on a pull request.
    pub fn disable_auto_merge(&self, pr: &IssueRef) -> Result<()> {
        let mut cmd = self.gh();
        cmd.args([
            "pr",
            "merge",
            &pr.number.to_string(),
            "--repo",
            &pr.repo,
            "--disable-auto",
        ]);
        let (ok, _, err) = self.run(cmd, None)?;
        if !ok {
            bail!("turning auto-merge off on {pr}: {}", err.trim());
        }
        Ok(())
    }

    /// The check rollup of a pull request's head commit.
    pub fn checks(&self, pr: &IssueRef) -> Result<Checks> {
        let (owner, name) = pr.repo.split_once('/').context("repo is not owner/name")?;
        let query = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(last: 1) { nodes { commit { oid statusCheckRollup { state
        contexts(first: 100) { nodes {
          __typename
          ... on CheckRun { name conclusion detailsUrl }
          ... on StatusContext { context state targetUrl }
        } }
      } } } }
    }
  }
}"#;
        let vars = json!({ "owner": owner, "name": name, "number": pr.number });
        let (data, errors) = self.graphql(query, vars)?;
        let commit = &data["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"];
        parse_checks(commit).with_context(|| format!("{pr}: no head commit; errors: {errors:?}"))
    }

    /// Whether an issue or pull request is open, and its labels. `None` when
    /// it doesn't exist.
    pub fn state_and_labels(&self, r: &IssueRef) -> Result<Option<(bool, Vec<String>)>> {
        let (owner, name) = r.repo.split_once('/').unwrap_or(("", &r.repo));
        let fields = "state labels(first: 50) { nodes { name } }";
        let q = format!(
            "query {{ repository(owner: {owner:?}, name: {name:?}) {{ issueOrPullRequest(number: {n}) {{ \
             ... on Issue {{ {fields} }} ... on PullRequest {{ {fields} }} }} }} }}",
            n = r.number
        );
        let (data, _) = self.graphql(&q, json!({}))?;
        let v = &data["repository"]["issueOrPullRequest"];
        let Some(state) = v["state"].as_str() else {
            return Ok(None);
        };
        let labels = nodes(v, "labels")
            .iter()
            .filter_map(|l| l["name"].as_str().map(String::from))
            .collect();
        Ok(Some((state == "OPEN", labels)))
    }

    /// GET a REST path as JSON. `Ok(None)` on 404.
    fn rest(&self, path: &str) -> Result<Option<Value>> {
        let mut cmd = self.gh();
        cmd.args(["api", path]);
        let (ok, out, err) = self.run(cmd, None)?;
        if ok {
            return Ok(Some(serde_json::from_str(&out)?));
        }
        if err.contains("HTTP 404") || out.contains("\"status\":\"404\"") {
            return Ok(None);
        }
        bail!("gh api {path}: {}", err.trim())
    }

    /// What decides whether `gh pr merge --auto` waits for anything in `repo`.
    pub fn merge_gates(&self, repo: &str) -> Result<MergeGates> {
        let r = self
            .rest(&format!("repos/{repo}"))?
            .with_context(|| format!("{repo} not found"))?;
        let branch = r["default_branch"]
            .as_str()
            .context("no default_branch")?
            .to_string();
        let protection = self.rest(&format!("repos/{repo}/branches/{branch}/protection"))?;
        let rules = self.rest(&format!("repos/{repo}/rules/branches/{branch}"))?;
        let repo_methods = [
            ("allow_merge_commit", "merge"),
            ("allow_squash_merge", "squash"),
            ("allow_rebase_merge", "rebase"),
        ]
        .iter()
        .map(|(k, m)| r[*k].as_bool().map(|on| on.then(|| m.to_string())))
        .collect::<Option<Vec<_>>>()
        .map(|v| v.into_iter().flatten().collect());
        Ok(MergeGates {
            allow_auto_merge: r["allow_auto_merge"].as_bool(),
            repo_methods,
            ..gates_of(branch, protection.as_ref(), rules.as_ref())
        })
    }

    /// Whether `login`'s latest review of the PR approves exactly `sha`.
    pub fn approved_at(&self, pr: &IssueRef, login: &str, sha: &str) -> Result<bool> {
        let mut cmd = self.gh();
        cmd.args([
            "api",
            "--paginate",
            &format!("repos/{}/pulls/{}/reviews", pr.repo, pr.number),
            "--jq",
            ".[] | {login: .user.login, state, commit_id}",
        ]);
        let (ok, out, err) = self.run(cmd, None)?;
        if !ok {
            bail!("reading the reviews of {pr}: {}", err.trim());
        }
        let reviews: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        Ok(latest_verdict_approves(&reviews, login, sha))
    }

    /// Head commits of the merged PRs from `branch` in `repo`.
    pub fn merged_pr_heads(&self, repo: &str, branch: &str) -> Result<Vec<String>> {
        let mut cmd = self.gh();
        cmd.args([
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "merged",
            "--limit",
            "100",
            "--json",
            "headRefOid",
            "--jq",
            ".[].headRefOid",
        ]);
        let (ok, out, err) = self.run(cmd, None)?;
        if !ok {
            bail!("listing merged PRs from {branch} in {repo}: {}", err.trim());
        }
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// Merges the PR at `head_sha`, refused when the branch moved since the
    /// review. Hands it to GitHub's auto-merge, which merges once the branch
    /// protection rules pass. GitHub won't enable auto-merge on a PR that can
    /// merge already ("clean status"); that one is merged at once.
    pub fn merge_pr(&self, pr: &IssueRef, method: MergeMethod, head_sha: &str) -> Result<Merged> {
        let merge = |auto: bool| -> Result<(bool, String)> {
            let mut cmd = self.gh();
            cmd.args(["pr", "merge", &pr.number.to_string(), "--repo", &pr.repo]);
            if auto {
                cmd.arg("--auto");
            }
            cmd.args([method.flag(), "--match-head-commit", head_sha]);
            let (ok, _, err) = self.run(cmd, None)?;
            Ok((ok, err.trim().to_string()))
        };
        match merge(true)? {
            (true, _) => Ok(Merged::Auto),
            (false, err) if err.contains("clean status") => match merge(false)? {
                (true, _) => Ok(Merged::Now),
                (false, err) => bail!("merging {pr}: {err}"),
            },
            (false, err) => bail!("merging {pr}: {err}"),
        }
    }

    /// Creates or updates the `dispatch:*` labels in a repository.
    pub fn ensure_labels(&self, repo: &str) -> Result<()> {
        let defs = [
            (READY, "0e8a16", "Released for a dispatched session"),
            (RUNNING, "fbca04", "A dispatched session is working on it"),
            (
                NEEDS_INPUT,
                "d93f0b",
                "Waiting for a human; see the latest comment",
            ),
            (
                CI,
                "c5def5",
                "PR open; issuewright waits for its checks, then reviews or resumes the session",
            ),
            (
                REVIEW,
                "1d76db",
                "PR open; the dispatched session is finished",
            ),
            (
                CHANGES,
                "e99695",
                "The reviewer requested changes; issuewright resumes the PR's author to make them",
            ),
            (APPROVED, "0e8a16", "The reviewer session approved the PR"),
            (
                MERGING,
                "5319e7",
                "issuewright enabled auto-merge on the approved PR",
            ),
        ];
        for (name, color, desc) in defs {
            let mut cmd = self.gh();
            cmd.args([
                "label",
                "create",
                name,
                "--repo",
                repo,
                "--color",
                color,
                "--description",
                desc,
                "--force",
            ]);
            let (ok, _, err) = self.run(cmd, None)?;
            if !ok {
                bail!("creating label {name} in {repo}: {}", err.trim());
            }
        }
        Ok(())
    }
}

/// Just enough to name an issue. Statuses of blockers and epics' sub-issues
/// are fetched separately, once each: nesting them in the issues query
/// repeats them for every issue and multiplies the query's rate-limit cost
/// (about 67 points per repository and poll; now about 2).
const REF_FIELDS: &str = "number repository { nameWithOwner }";

const STATUS_FIELDS: &str = r#"
  number title state stateReason createdAt closedAt
  repository { nameWithOwner }
  labels(first: 50) { nodes { name } }
  closedByPullRequestsReferences(first: 10, includeClosedPrs: true) {
    nodes { number merged baseRefName repository { nameWithOwner } baseRepository { defaultBranchRef { name } } }
  }
"#;

fn issues_query() -> String {
    format!(
        r#"query($owner: String!, $name: String!, $labels: [String!], $after: String) {{
  repository(owner: $owner, name: $name) {{
    issues(first: 25, after: $after, states: OPEN, labels: $labels) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        {STATUS_FIELDS}
        body
        subIssuesSummary {{ total completed }}
        parent {{ {STATUS_FIELDS} }}
        blockedBy(first: 50) {{ nodes {{ {REF_FIELDS} }} }}
      }}
    }}
  }}
}}"#
    )
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str()
}

fn time_at(v: &Value, key: &str) -> Result<Option<DateTime<Utc>>> {
    match v.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(s) => Ok(Some(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))),
    }
}

fn nodes(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(|x| x.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// What makes `gh pr merge --auto` wait in a repository. `None`: unknown,
/// for example because the token may not read the setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeGates {
    pub default_branch: String,
    pub allow_auto_merge: Option<bool>,
    /// Branch protection or a ruleset requires status checks.
    pub required_checks: bool,
    /// Approving reviews required, the higher of protection and rulesets.
    pub required_reviews: u64,
    /// The merge methods the repository's settings allow; `None` when they
    /// couldn't be read.
    pub repo_methods: Option<Vec<String>>,
    /// The merge methods every ruleset on the default branch allows; `None`
    /// when no ruleset restricts them.
    pub ruleset_methods: Option<Vec<String>>,
}

impl MergeGates {
    /// Whether GitHub accepts `method` for a PR into the default branch.
    /// Auto-merge with a method it doesn't accept is enabled anyway, but
    /// never merges.
    pub fn allows(&self, method: MergeMethod) -> bool {
        let n = method.name();
        [&self.repo_methods, &self.ruleset_methods]
            .into_iter()
            .flatten()
            .all(|ms| ms.iter().any(|m| m == n))
    }

    /// The methods both the settings and the rulesets allow, for messages.
    pub fn allowed_methods(&self) -> Vec<&'static str> {
        [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase]
            .into_iter()
            .filter(|m| self.allows(*m))
            .map(MergeMethod::name)
            .collect()
    }

    /// Something to wait for before merging: checks or reviews.
    pub fn gated(&self) -> bool {
        self.required_checks || self.required_reviews > 0
    }
}

/// Reads classic branch protection and the rules that apply to the branch.
pub fn gates_of(
    default_branch: String,
    protection: Option<&Value>,
    rules: Option<&Value>,
) -> MergeGates {
    let mut g = MergeGates {
        default_branch,
        ..Default::default()
    };
    if let Some(p) = protection {
        let checks = &p["required_status_checks"];
        g.required_checks |= ["contexts", "checks"]
            .iter()
            .any(|k| checks[k].as_array().is_some_and(|a| !a.is_empty()));
        if let Some(n) =
            p["required_pull_request_reviews"]["required_approving_review_count"].as_u64()
        {
            g.required_reviews = g.required_reviews.max(n);
        }
    }
    for rule in rules.and_then(Value::as_array).into_iter().flatten() {
        let params = &rule["parameters"];
        match rule["type"].as_str() {
            Some("required_status_checks") => {
                g.required_checks |= params["required_status_checks"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty());
            }
            Some("pull_request") => {
                if let Some(n) = params["required_approving_review_count"].as_u64() {
                    g.required_reviews = g.required_reviews.max(n);
                }
                // Each ruleset narrows the methods further.
                if let Some(ms) = params["allowed_merge_methods"].as_array() {
                    let ms: Vec<String> = ms
                        .iter()
                        .filter_map(|m| m.as_str().map(String::from))
                        .collect();
                    g.ruleset_methods = Some(match g.ruleset_methods.take() {
                        Some(prev) => prev.into_iter().filter(|m| ms.contains(m)).collect(),
                        None => ms,
                    });
                }
            }
            _ => {}
        }
    }
    g
}

/// Whether `login`'s latest verdict (approve, request changes, dismissed;
/// plain comments don't count) approves `sha`. `reviews` is oldest first.
pub fn latest_verdict_approves(reviews: &[Value], login: &str, sha: &str) -> bool {
    reviews
        .iter()
        .rfind(|r| r["login"] == login && r["state"] != "COMMENTED" && r["state"] != "PENDING")
        .is_some_and(|r| r["state"] == "APPROVED" && r["commit_id"] == sha)
}

/// Picks the open PR from the issue's repository that targets its default
/// branch out of `closedByPullRequestsReferences`.
pub fn parse_open_pr(issue_node: &Value, repo: &str) -> Option<PullRequest> {
    nodes(issue_node, "closedByPullRequestsReferences")
        .iter()
        .find_map(|pr| {
            let open = str_at(pr, &["state"]) == Some("OPEN");
            let pr_repo = str_at(pr, &["repository", "nameWithOwner"])?;
            let base = str_at(pr, &["baseRefName"])?;
            let default = str_at(pr, &["baseRepository", "defaultBranchRef", "name"])?;
            (open && pr_repo.eq_ignore_ascii_case(repo) && base == default).then(|| PullRequest {
                r: IssueRef::new(
                    pr_repo,
                    pr.get("number").and_then(Value::as_u64).unwrap_or(0),
                ),
                head_ref: str_at(pr, &["headRefName"]).unwrap_or_default().to_string(),
                head_sha: str_at(pr, &["headRefOid"]).unwrap_or_default().to_string(),
                draft: pr.get("isDraft").and_then(Value::as_bool) == Some(true),
            })
        })
}

/// The check rollup of a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checks {
    pub head_sha: String,
    /// `SUCCESS`, `FAILURE`, `ERROR`, `PENDING` or `EXPECTED`; `None` while
    /// the commit has no checks at all.
    pub state: Option<String>,
    /// The failed checks, as `name: url`.
    pub failed: Vec<String>,
}

pub fn parse_checks(commit: &Value) -> Option<Checks> {
    let head_sha = str_at(commit, &["oid"])?.to_string();
    let rollup = &commit["statusCheckRollup"];
    let failed = nodes(rollup, "contexts")
        .iter()
        .filter_map(|c| {
            let (name, bad, url) = match str_at(c, &["__typename"]) {
                Some("CheckRun") => (
                    str_at(c, &["name"])?,
                    matches!(
                        str_at(c, &["conclusion"]),
                        Some(
                            "FAILURE"
                                | "TIMED_OUT"
                                | "CANCELLED"
                                | "ACTION_REQUIRED"
                                | "STARTUP_FAILURE"
                        )
                    ),
                    str_at(c, &["detailsUrl"]),
                ),
                Some("StatusContext") => (
                    str_at(c, &["context"])?,
                    matches!(str_at(c, &["state"]), Some("FAILURE" | "ERROR")),
                    str_at(c, &["targetUrl"]),
                ),
                _ => return None,
            };
            bad.then(|| format!("{name}: {}", url.unwrap_or("(no link)")))
        })
        .collect();
    Some(Checks {
        head_sha,
        state: str_at(rollup, &["state"]).map(String::from),
        failed,
    })
}

const BRIEF_FIELDS: &str = "title state stateReason labels(first: 50) { nodes { name } } subIssuesSummary { total completed }";

/// What issuewright needs to know about an issue outside the snapshot: a
/// session's issue that left it, or an epic whose sub-issues may all be closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueBrief {
    pub r: IssueRef,
    pub title: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub sub_issues: u64,
    pub sub_issues_closed: u64,
}

impl IssueBrief {
    pub fn has_label(&self, l: &str) -> bool {
        self.labels.iter().any(|x| x.eq_ignore_ascii_case(l))
    }
    pub fn open_sub_issues(&self) -> u64 {
        self.sub_issues.saturating_sub(self.sub_issues_closed)
    }
}

fn parse_brief(r: &IssueRef, v: &Value) -> Option<IssueBrief> {
    Some(IssueBrief {
        r: r.clone(),
        title: str_at(v, &["title"]).unwrap_or_default().to_string(),
        state: parse_state(v)?,
        labels: nodes(v, "labels")
            .iter()
            .filter_map(|l| l["name"].as_str().map(String::from))
            .collect(),
        sub_issues: v["subIssuesSummary"]["total"].as_u64().unwrap_or(0),
        sub_issues_closed: v["subIssuesSummary"]["completed"].as_u64().unwrap_or(0),
    })
}

/// An issue's `state` and `stateReason`.
fn parse_state(v: &Value) -> Option<IssueState> {
    match (str_at(v, &["state"]), str_at(v, &["stateReason"])) {
        (Some("OPEN"), _) => Some(IssueState::Open),
        (Some("CLOSED"), Some("COMPLETED")) => Some(IssueState::ClosedCompleted),
        (Some("CLOSED"), _) => Some(IssueState::ClosedOther),
        _ => None,
    }
}

/// Parses the fields of `STATUS_FIELDS` on an issue.
pub fn parse_status(v: &Value) -> Result<IssueStatus> {
    let repo = str_at(v, &["repository", "nameWithOwner"]).context("issue without repository")?;
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .context("issue without number")?;
    let state = parse_state(v)
        .with_context(|| format!("{repo}#{number}: unknown state {:?}", str_at(v, &["state"])))?;
    let labels = nodes(v, "labels")
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let merged_pr = nodes(v, "closedByPullRequestsReferences")
        .iter()
        .find_map(|pr| {
            let merged = pr.get("merged").and_then(Value::as_bool) == Some(true);
            let base = str_at(pr, &["baseRefName"])?;
            let default = str_at(pr, &["baseRepository", "defaultBranchRef", "name"])?;
            let pr_repo = str_at(pr, &["repository", "nameWithOwner"])?;
            (merged && base == default).then(|| {
                IssueRef::new(
                    pr_repo,
                    pr.get("number").and_then(Value::as_u64).unwrap_or(0),
                )
            })
        });
    Ok(IssueStatus {
        r: IssueRef::new(repo, number),
        title: str_at(v, &["title"]).unwrap_or_default().to_string(),
        state,
        labels,
        created_at: time_at(v, "createdAt")?.context("issue without createdAt")?,
        closed_at: time_at(v, "closedAt")?,
        merged_pr,
    })
}

/// Parses one issue node of `issues_query` into the snapshot.
pub fn absorb_issue_node(snap: &mut Snapshot, v: &Value) -> Result<()> {
    let status = parse_status(v)?;
    let parent = match v.get("parent") {
        Some(p) if !p.is_null() => {
            let ps = parse_status(p)?;
            let r = ps.r.clone();
            // Sub-issues come later, from `fetch_sub_issues`.
            snap.epics.entry(r.key()).or_insert(Epic {
                status: ps,
                sub_issues: Vec::new(),
            });
            Some(r)
        }
        _ => None,
    };
    let mut native = Vec::new();
    for b in nodes(v, "blockedBy") {
        let repo =
            str_at(&b, &["repository", "nameWithOwner"]).context("blocker without repository")?;
        let number = b
            .get("number")
            .and_then(Value::as_u64)
            .context("blocker without number")?;
        native.push(IssueRef::new(repo, number));
    }
    let body = str_at(v, &["body"]).unwrap_or_default();
    let body_blockers = dependency_refs(body, &status.r.repo);
    let summary = |k: &str| v["subIssuesSummary"][k].as_u64().unwrap_or(0);
    snap.issues.push(Issue {
        status,
        parent,
        sub_issue_count: summary("total"),
        sub_issues_completed: summary("completed"),
        native_blockers: native,
        body_blockers,
    });
    Ok(())
}

/// Builds the snapshot for the given `owner/name` repositories.
pub fn fetch_snapshot(gh: &Gh, repos: &[String]) -> Result<Snapshot> {
    fetch_snapshot_with(gh, repos, &ALL_LABELS)
}

fn fetch_snapshot_with(gh: &Gh, repos: &[String], labels: &[&str]) -> Result<Snapshot> {
    let mut snap = Snapshot::default();
    let query = issues_query();
    for repo in repos {
        let (owner, name) = repo.split_once('/').context("repo is not owner/name")?;
        let mut after: Option<String> = None;
        loop {
            let vars = json!({ "owner": owner, "name": name, "labels": labels, "after": after });
            let (data, errors) = gh.graphql(&query, vars)?;
            let issues = data
                .get("repository")
                .and_then(|r| r.get("issues"))
                .with_context(|| format!("{repo}: no issues in response; errors: {errors:?}"))?;
            for n in issues
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                absorb_issue_node(&mut snap, &n)
                    .with_context(|| format!("parsing an issue of {repo}"))?;
            }
            let page = &issues["pageInfo"];
            if page["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            after = page["endCursor"].as_str().map(String::from);
        }
    }
    for repo in repos {
        snap.prs.extend(fetch_labeled_prs(gh, repo)?);
    }
    fetch_sub_issues(gh, &mut snap)?;
    resolve_blockers(gh, &mut snap)?;
    Ok(snap)
}

/// Open pull requests of `repo` labeled `dispatch:review` or
/// `dispatch:approved`. Agents outside issuewright label the PR instead of its
/// issue, and a PR that closes no issue is reviewed on its own.
fn fetch_labeled_prs(gh: &Gh, repo: &str) -> Result<Vec<LabeledPr>> {
    let (owner, name) = repo.split_once('/').context("repo is not owner/name")?;
    let query = r#"query($owner: String!, $name: String!, $labels: [String!]) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 50, states: OPEN, labels: $labels) {
      nodes {
        number title createdAt isDraft headRefName headRefOid baseRefName
        labels(first: 50) { nodes { name } }
        headRepository { nameWithOwner }
        baseRepository { defaultBranchRef { name } }
        closingIssuesReferences(first: 10) { nodes { number state repository { nameWithOwner } } }
      }
    }
  }
}"#;
    let vars = json!({ "owner": owner, "name": name, "labels": [REVIEW, APPROVED] });
    let (data, errors) = gh.graphql(query, vars)?;
    let node = data
        .get("repository")
        .filter(|r| !r.is_null())
        .with_context(|| format!("{repo}: not found; errors: {errors:?}"))?;
    nodes(node, "pullRequests")
        .iter()
        .map(|pr| parse_labeled_pr(pr, repo))
        .collect::<Result<_>>()
        .with_context(|| format!("parsing a pull request of {repo}"))
}

pub fn parse_labeled_pr(v: &Value, repo: &str) -> Result<LabeledPr> {
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .context("no number")?;
    let r = IssueRef::new(repo, number);
    let closes = nodes(v, "closingIssuesReferences")
        .iter()
        .filter(|i| str_at(i, &["state"]) == Some("OPEN"))
        .filter(|i| {
            str_at(i, &["repository", "nameWithOwner"])
                .is_some_and(|x| x.eq_ignore_ascii_case(repo))
        })
        .filter_map(|i| i.get("number").and_then(Value::as_u64))
        .map(|n| IssueRef::new(repo, n))
        .collect();
    let own = str_at(v, &["headRepository", "nameWithOwner"])
        .is_some_and(|x| x.eq_ignore_ascii_case(repo));
    let into_default = str_at(v, &["baseRefName"]).is_some()
        && str_at(v, &["baseRefName"])
            == str_at(v, &["baseRepository", "defaultBranchRef", "name"]);
    Ok(LabeledPr {
        status: IssueStatus {
            r: r.clone(),
            title: str_at(v, &["title"]).unwrap_or_default().to_string(),
            state: IssueState::Open,
            labels: nodes(v, "labels")
                .iter()
                .filter_map(|l| str_at(l, &["name"]).map(String::from))
                .collect(),
            created_at: time_at(v, "createdAt")?.context("no createdAt")?,
            closed_at: None,
            merged_pr: None,
        },
        pr: PullRequest {
            r,
            head_ref: str_at(v, &["headRefName"]).unwrap_or_default().to_string(),
            head_sha: str_at(v, &["headRefOid"]).unwrap_or_default().to_string(),
            draft: v.get("isDraft").and_then(Value::as_bool) == Some(true),
        },
        closes,
        own_into_default: own && into_default,
    })
}

/// Fills in the sub-issues of every epic in the snapshot, a few epics per
/// query.
fn fetch_sub_issues(gh: &Gh, snap: &mut Snapshot) -> Result<()> {
    let epics: Vec<IssueRef> = snap.epics.values().map(|e| e.status.r.clone()).collect();
    for chunk in epics.chunks(5) {
        let mut q = String::from("query {\n");
        for (i, r) in chunk.iter().enumerate() {
            let (owner, name) = r.repo.split_once('/').unwrap_or(("", &r.repo));
            q.push_str(&format!(
                "  e{i}: repository(owner: {owner:?}, name: {name:?}) {{ issue(number: {n}) {{ subIssues(first: 100) {{ nodes {{ {STATUS_FIELDS} }} }} }} }}\n",
                n = r.number
            ));
        }
        q.push('}');
        let (data, errors) = gh.graphql(&q, json!({}))?;
        for (i, r) in chunk.iter().enumerate() {
            let Some(issue) = data
                .get(format!("e{i}"))
                .and_then(|v| v.get("issue"))
                .filter(|v| !v.is_null())
            else {
                bail!("epic {r}: sub-issues not readable; errors: {errors:?}");
            };
            let subs = nodes(issue, "subIssues")
                .iter()
                .map(parse_status)
                .collect::<Result<Vec<_>>>()?;
            if let Some(e) = snap.epics.get_mut(&r.key()) {
                e.sub_issues = subs;
            }
        }
    }
    Ok(())
}

/// Looks up the status of every blocker (native or from the body) the
/// snapshot doesn't hold yet, once per blocker. A body reference may also
/// point at a pull request; it counts as done once merged.
fn resolve_blockers(gh: &Gh, snap: &mut Snapshot) -> Result<()> {
    let mut wanted: BTreeMap<(String, u64), IssueRef> = BTreeMap::new();
    for i in &snap.issues {
        for b in i.native_blockers.iter().chain(&i.body_blockers) {
            if snap.status(b).is_none() {
                wanted.insert(b.key(), b.clone());
            }
        }
    }
    let wanted: Vec<IssueRef> = wanted.into_values().collect();
    for chunk in wanted.chunks(20) {
        let mut q = String::from("query {\n");
        for (i, r) in chunk.iter().enumerate() {
            let (owner, name) = r.repo.split_once('/').unwrap_or(("", &r.repo));
            q.push_str(&format!(
                "  b{i}: repository(owner: {owner:?}, name: {name:?}) {{ issueOrPullRequest(number: {n}) {{\n    __typename\n    ... on Issue {{ {STATUS_FIELDS} }}\n    ... on PullRequest {{ number title state merged createdAt closedAt baseRefName repository {{ nameWithOwner }} baseRepository {{ defaultBranchRef {{ name }} }} }}\n  }} }}\n",
                n = r.number
            ));
        }
        q.push('}');
        let (data, errors) = gh.graphql(&q, json!({}))?;
        for (i, r) in chunk.iter().enumerate() {
            let alias = format!("b{i}");
            let repo_v = data.get(&alias);
            let node = repo_v.and_then(|v| v.get("issueOrPullRequest"));
            match (repo_v, node) {
                (None, _) | (Some(Value::Null), _) => {
                    snap.unresolved
                        .insert(r.key(), "repository not found or not readable".into());
                }
                (_, None) | (_, Some(Value::Null)) => {
                    snap.unresolved
                        .insert(r.key(), "no such issue or pull request".into());
                }
                (_, Some(n)) if n["__typename"] == "PullRequest" => {
                    snap.others.insert(r.key(), parse_pull_request(n)?);
                }
                (_, Some(n)) => {
                    snap.others.insert(r.key(), parse_status(n)?);
                }
            }
        }
        if !errors.is_empty() {
            let unexpected: Vec<&Value> = errors
                .iter()
                .filter(|e| e.get("type").and_then(Value::as_str) != Some("NOT_FOUND"))
                .collect();
            if !unexpected.is_empty() {
                eprintln!("issuewright: blocker lookup errors: {unexpected:?}");
            }
        }
    }
    Ok(())
}

fn parse_pull_request(v: &Value) -> Result<IssueStatus> {
    let repo = str_at(v, &["repository", "nameWithOwner"]).context("PR without repository")?;
    let number = v["number"].as_u64().context("PR without number")?;
    let merged_into_default = v["merged"].as_bool() == Some(true)
        && str_at(v, &["baseRefName"])
            == str_at(v, &["baseRepository", "defaultBranchRef", "name"]);
    let state = match str_at(v, &["state"]) {
        Some("OPEN") => IssueState::Open,
        _ if merged_into_default => IssueState::ClosedCompleted,
        _ => IssueState::ClosedOther,
    };
    Ok(IssueStatus {
        r: IssueRef::new(repo, number),
        title: str_at(v, &["title"]).unwrap_or_default().to_string(),
        state,
        labels: vec![],
        created_at: time_at(v, "createdAt")?.context("PR without createdAt")?,
        closed_at: time_at(v, "closedAt")?,
        merged_pr: merged_into_default.then(|| IssueRef::new(repo, number)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_list_the_failed_ones() {
        let commit = json!({
            "oid": "abc",
            "statusCheckRollup": { "state": "FAILURE", "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "test", "conclusion": "SUCCESS", "detailsUrl": "u1" },
                { "__typename": "CheckRun", "name": "lint", "conclusion": "FAILURE", "detailsUrl": "u2" },
                { "__typename": "CheckRun", "name": "slow", "conclusion": "TIMED_OUT", "detailsUrl": null },
                { "__typename": "StatusContext", "context": "ext", "state": "ERROR", "targetUrl": "u4" },
                { "__typename": "CheckRun", "name": "skip", "conclusion": "SKIPPED", "detailsUrl": "u5" }
            ] } }
        });
        let c = parse_checks(&commit).unwrap();
        assert_eq!(c.head_sha, "abc");
        assert_eq!(c.state.as_deref(), Some("FAILURE"));
        assert_eq!(c.failed, ["lint: u2", "slow: (no link)", "ext: u4"]);
        let none = parse_checks(&json!({ "oid": "def", "statusCheckRollup": null })).unwrap();
        assert_eq!((none.state, none.failed.len()), (None, 0));
    }

    /// A stub `gh` that logs its arguments and answers `pr merge` like
    /// GitHub: `--auto` on a clean PR fails, a moved head fails.
    fn stub_gh(dir: &std::path::Path, clean: bool) -> Gh {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let bin = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{log}"
case " $* " in *" --auto "*) [ "{clean}" = true ] && {{ echo "GraphQL: Pull request Pull request is in clean status (enablePullRequestAutoMerge)" >&2; exit 1; }};; esac
case " $* " in *" --match-head-commit moved "*) echo "Head branch was modified" >&2; exit 1;; esac
exit 0
"#,
            log = dir.join("calls").display(),
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut gh = Gh::with_token("t".into());
        gh.bin = bin.display().to_string();
        gh
    }

    fn calls(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn merge_falls_back_to_a_direct_merge_when_clean() {
        let root = std::env::temp_dir().join(format!("issuewright-merge-{}", std::process::id()));
        let pr = IssueRef {
            repo: "o/r".into(),
            number: 7,
        };
        let m = MergeMethod::Squash;

        let pending = root.join("pending");
        let how = stub_gh(&pending, false).merge_pr(&pr, m, "abc").unwrap();
        assert_eq!(how, Merged::Auto);
        assert_eq!(
            calls(&pending),
            ["pr merge 7 --repo o/r --auto --squash --match-head-commit abc"]
        );

        let clean = root.join("clean");
        let how = stub_gh(&clean, true).merge_pr(&pr, m, "abc").unwrap();
        assert_eq!(how, Merged::Now);
        assert_eq!(
            calls(&clean),
            [
                "pr merge 7 --repo o/r --auto --squash --match-head-commit abc",
                "pr merge 7 --repo o/r --squash --match-head-commit abc",
            ]
        );

        let moved = root.join("moved");
        let err = stub_gh(&moved, true).merge_pr(&pr, m, "moved").unwrap_err();
        assert!(
            format!("{err:#}").contains("Head branch was modified"),
            "{err:#}"
        );
        // The direct merge keeps --match-head-commit, so GitHub refuses it.
        assert_eq!(
            calls(&moved).last().unwrap(),
            "pr merge 7 --repo o/r --squash --match-head-commit moved"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Live check of the queries against GitHub: `ISSUEWRIGHT_LIVE_REPO=owner/name
    /// ISSUEWRIGHT_LIVE_LABEL=some-label cargo test live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_snapshot() {
        let repo = std::env::var("ISSUEWRIGHT_LIVE_REPO").unwrap();
        let label = std::env::var("ISSUEWRIGHT_LIVE_LABEL").unwrap();
        let gh = Gh::new(std::env::var("ISSUEWRIGHT_LIVE_GH_USER").ok().as_deref()).unwrap();
        let snap = fetch_snapshot_with(&gh, &[repo], &[label.as_str()]).unwrap();
        for i in &snap.issues {
            println!(
                "{} epic={:?} subs={} native={:?} body={:?} merged_pr={:?}",
                i.r(),
                i.parent.as_ref().map(|p| p.to_string()),
                i.sub_issue_count,
                i.native_blockers
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>(),
                i.body_blockers
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>(),
                i.status.merged_pr.as_ref().map(|p| p.to_string()),
            );
        }
        println!(
            "epics={} others={} unresolved={:?}",
            snap.epics.len(),
            snap.others.len(),
            snap.unresolved
        );
    }

    fn status_json(
        repo: &str,
        n: u64,
        state: &str,
        reason: Option<&str>,
        labels: &[&str],
    ) -> Value {
        json!({
            "number": n, "title": format!("t{n}"), "state": state, "stateReason": reason,
            "createdAt": "2026-11-01T10:00:00Z", "closedAt": null,
            "repository": { "nameWithOwner": repo },
            "labels": { "nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>() },
            "closedByPullRequestsReferences": { "nodes": [] }
        })
    }

    #[test]
    fn merged_pr_counts_only_into_default_branch() {
        let mut v = status_json("acme/app", 1, "OPEN", None, &[]);
        v["closedByPullRequestsReferences"]["nodes"] = json!([
            { "number": 5, "merged": true, "baseRefName": "feature", "repository": {"nameWithOwner": "acme/app"},
              "baseRepository": {"defaultBranchRef": {"name": "main"}} },
            { "number": 6, "merged": false, "baseRefName": "main", "repository": {"nameWithOwner": "acme/app"},
              "baseRepository": {"defaultBranchRef": {"name": "main"}} }
        ]);
        assert_eq!(parse_status(&v).unwrap().merged_pr, None);
        v["closedByPullRequestsReferences"]["nodes"][1]["merged"] = json!(true);
        assert_eq!(
            parse_status(&v).unwrap().merged_pr,
            Some(IssueRef::new("acme/app", 6))
        );
    }

    #[test]
    fn labeled_pr_closes_only_open_issues_of_its_repo() {
        let issue = |n: u64, repo: &str, state: &str| json!({"number": n, "state": state, "repository": {"nameWithOwner": repo}});
        let pr = |head_repo: &str, base: &str| {
            json!({"number": 7, "title": "t", "createdAt": "2026-11-17T10:00:00Z", "isDraft": false,
            "headRefName": "feat/7", "headRefOid": "sha7", "baseRefName": base,
            "labels": {"nodes": [{"name": "dispatch:review"}]},
            "headRepository": {"nameWithOwner": head_repo},
            "baseRepository": {"defaultBranchRef": {"name": "main"}},
            "closingIssuesReferences": {"nodes": [
                issue(1, "acme/mono", "OPEN"),
                issue(2, "acme/mono", "CLOSED"),
                issue(3, "acme/other", "OPEN"),
            ]}})
        };
        let p = parse_labeled_pr(&pr("acme/mono", "main"), "acme/mono").unwrap();
        assert_eq!(p.closes, [IssueRef::new("acme/mono", 1)]);
        assert_eq!(p.pr.r, IssueRef::new("acme/mono", 7));
        assert_eq!(p.pr.head_sha, "sha7");
        assert!(p.status.has_label(REVIEW));
        assert!(p.own_into_default);
        assert!(
            !parse_labeled_pr(&pr("fork/mono", "main"), "acme/mono")
                .unwrap()
                .own_into_default
        );
        assert!(
            !parse_labeled_pr(&pr("acme/mono", "release"), "acme/mono")
                .unwrap()
                .own_into_default
        );
    }

    #[test]
    fn open_pr_only_from_own_repo_into_default_branch() {
        let pr = |n: u64, repo: &str, state: &str, base: &str| {
            json!({"number": n, "state": state, "isDraft": false, "headRefName": format!("feat/{n}"),
                   "headRefOid": format!("sha{n}"), "baseRefName": base,
                   "repository": {"nameWithOwner": repo},
                   "baseRepository": {"defaultBranchRef": {"name": "main"}}})
        };
        let v = json!({"closedByPullRequestsReferences": {"nodes": [
            pr(1, "acme/mono", "MERGED", "main"),
            pr(2, "acme/other", "OPEN", "main"),
            pr(3, "acme/mono", "OPEN", "release"),
            pr(4, "acme/mono", "OPEN", "main"),
        ]}});
        let got = parse_open_pr(&v, "acme/mono").unwrap();
        assert_eq!(got.r, IssueRef::new("acme/mono", 4));
        assert_eq!(got.head_sha, "sha4");
        assert_eq!(got.head_ref, "feat/4");
        assert!(parse_open_pr(&json!({}), "acme/mono").is_none());
    }

    #[test]
    fn gates_from_protection_and_rulesets() {
        let none = gates_of("main".into(), None, Some(&json!([{"type": "deletion"}])));
        assert!(!none.gated());
        let classic = gates_of(
            "main".into(),
            Some(&json!({"required_status_checks": {"contexts": ["ci"]}})),
            None,
        );
        assert!(classic.required_checks && classic.gated());
        let ruleset = gates_of(
            "main".into(),
            None,
            Some(&json!([
                {"type": "pull_request", "parameters": {"required_approving_review_count": 1}},
                {"type": "required_status_checks", "parameters": {"required_status_checks": []}}
            ])),
        );
        assert_eq!(ruleset.required_reviews, 1);
        assert!(!ruleset.required_checks);
        assert!(
            ruleset.allows(MergeMethod::Squash),
            "no ruleset restricts methods"
        );
    }

    #[test]
    fn merge_methods_from_settings_and_rulesets() {
        let mut g = gates_of(
            "master".into(),
            None,
            Some(&json!([
                {"type": "pull_request", "parameters": {"allowed_merge_methods": ["merge", "squash"]}},
                {"type": "pull_request", "parameters": {"allowed_merge_methods": ["merge"]}}
            ])),
        );
        assert!(g.allows(MergeMethod::Merge));
        assert!(
            !g.allows(MergeMethod::Squash),
            "the second ruleset narrows it"
        );
        g.repo_methods = Some(vec!["squash".into(), "rebase".into()]);
        assert!(
            !g.allows(MergeMethod::Merge),
            "the settings turn merge commits off"
        );
        assert!(g.allowed_methods().is_empty());
    }

    #[test]
    fn only_the_latest_verdict_on_the_reviewed_commit_counts() {
        let r = |login: &str, state: &str, sha: &str| json!({"login": login, "state": state, "commit_id": sha});
        let bot = "rev[bot]";
        assert!(latest_verdict_approves(
            &[r(bot, "APPROVED", "a")],
            bot,
            "a"
        ));
        // Another commit, another reviewer, or a later change request.
        assert!(!latest_verdict_approves(
            &[r(bot, "APPROVED", "a")],
            bot,
            "b"
        ));
        assert!(!latest_verdict_approves(
            &[r("someone", "APPROVED", "a")],
            bot,
            "a"
        ));
        assert!(!latest_verdict_approves(
            &[r(bot, "APPROVED", "a"), r(bot, "CHANGES_REQUESTED", "a")],
            bot,
            "a"
        ));
        // A comment after the approval doesn't undo it.
        assert!(latest_verdict_approves(
            &[r(bot, "APPROVED", "a"), r(bot, "COMMENTED", "a")],
            bot,
            "a"
        ));
        assert!(!latest_verdict_approves(&[], bot, "a"));
    }

    #[test]
    fn states() {
        let s = |st, r| {
            parse_status(&status_json("a/b", 1, st, r, &[]))
                .unwrap()
                .state
        };
        assert_eq!(s("OPEN", None), IssueState::Open);
        assert_eq!(s("CLOSED", Some("COMPLETED")), IssueState::ClosedCompleted);
        assert_eq!(s("CLOSED", Some("NOT_PLANNED")), IssueState::ClosedOther);
    }

    #[test]
    fn absorbs_parent_blockers_and_body() {
        let mut v = status_json("acme/web", 7, "OPEN", None, &[READY]);
        v["body"] = json!("### Dependencies / Blockers\n\n- acme/mono#3\n");
        v["subIssuesSummary"] = json!({"total": 3, "completed": 1});
        let parent = status_json("acme/mono", 1, "OPEN", None, &[]);
        v["parent"] = parent;
        v["blockedBy"] =
            json!({"nodes": [{"number": 2, "repository": {"nameWithOwner": "acme/mono"}}]});
        let mut snap = Snapshot::default();
        absorb_issue_node(&mut snap, &v).unwrap();
        let i = &snap.issues[0];
        assert_eq!(i.parent, Some(IssueRef::new("acme/mono", 1)));
        assert_eq!(i.native_blockers, [IssueRef::new("acme/mono", 2)]);
        assert_eq!(i.body_blockers, [IssueRef::new("acme/mono", 3)]);
        assert_eq!((i.sub_issue_count, i.sub_issues_completed), (3, 1));
        let b = parse_brief(
            i.r(),
            &json!({"title": "t", "state": "OPEN", "labels": {"nodes": [{"name": RUNNING}]},
                    "subIssuesSummary": {"total": 3, "completed": 1}}),
        )
        .unwrap();
        assert!(b.has_label(RUNNING));
        assert_eq!(b.open_sub_issues(), 2);
        // Sub-issues and blocker statuses are fetched in separate queries.
        assert!(
            snap.epics[&IssueRef::new("acme/mono", 1).key()]
                .sub_issues
                .is_empty()
        );
        assert!(snap.others.is_empty());
    }
}
