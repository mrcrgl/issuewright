//! A snapshot of the issues issuewright looks at. Built from GitHub by `github.rs`,
//! consumed by the pure scheduler in `scheduler.rs`.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const READY: &str = "dispatch:ready";
pub const RUNNING: &str = "dispatch:running";
pub const NEEDS_INPUT: &str = "dispatch:needs-input";
pub const REVIEW: &str = "dispatch:review";
/// PR open and pushed; issuewright waits for its checks, no session runs.
pub const CI: &str = "dispatch:ci";
/// Set by a reviewer session that issuewright started: the PR may be merged.
pub const APPROVED: &str = "dispatch:approved";
/// Set by issuewright once it handed the approved PR to GitHub's auto-merge.
pub const MERGING: &str = "dispatch:merging";
/// Set by a reviewer session that found defects the PR's author can fix
/// without a decision from you: issuewright resumes the session that wrote it.
pub const CHANGES: &str = "dispatch:changes-requested";
pub const ALL_LABELS: [&str; 8] = [
    READY,
    RUNNING,
    NEEDS_INPUT,
    CI,
    REVIEW,
    CHANGES,
    APPROVED,
    MERGING,
];

pub const STATUS_BLOCKED: &str = "status:blocked";
/// Marks an epic even before (or without) native sub-issues.
pub const TYPE_EPIC: &str = "type:epic";
pub const STATUS_IN_PROGRESS: &str = "status:in-progress";
pub const STATUS_REVIEW: &str = "status:review";

/// An issue (or pull request) by repository and number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IssueRef {
    /// `owner/name`, as GitHub spells it.
    pub repo: String,
    pub number: u64,
}

impl IssueRef {
    pub fn new(repo: impl Into<String>, number: u64) -> Self {
        Self {
            repo: repo.into(),
            number,
        }
    }

    /// Case-insensitive key; GitHub repository names are case-insensitive.
    pub fn key(&self) -> (String, u64) {
        (self.repo.to_lowercase(), self.number)
    }

    pub fn short(&self) -> String {
        let name = self.repo.rsplit('/').next().unwrap_or(&self.repo);
        format!("{name}#{}", self.number)
    }
}

impl fmt::Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueState {
    Open,
    ClosedCompleted,
    /// Closed as not planned or duplicate.
    ClosedOther,
}

/// What every issue in the snapshot carries, whether it is a candidate, a
/// blocker, an epic or a sibling sub-issue.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueStatus {
    pub r: IssueRef,
    pub title: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// A merged pull request that closes this issue and targets its
    /// repository's default branch.
    pub merged_pr: Option<IssueRef>,
}

impl IssueStatus {
    pub fn has_label(&self, l: &str) -> bool {
        self.labels.iter().any(|x| x.eq_ignore_ascii_case(l))
    }

    /// `priority:P0`..`priority:P4`, or the legacy `P0`..`P4`. Lower is more
    /// urgent; issues without a priority sort after P4.
    pub fn priority(&self) -> u8 {
        priority_of(&self.labels)
    }
}

pub fn priority_of(labels: &[String]) -> u8 {
    labels
        .iter()
        .filter_map(|l| {
            let l = l.strip_prefix("priority:").unwrap_or(l);
            let d = l.strip_prefix('P').or_else(|| l.strip_prefix('p'))?;
            match d.parse::<u8>() {
                Ok(n) if n <= 4 => Some(n),
                _ => None,
            }
        })
        .min()
        .unwrap_or(5)
}

/// An issue carrying one of the `dispatch:*` labels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub status: IssueStatus,
    pub parent: Option<IssueRef>,
    /// Number of sub-issues; more than zero makes this an epic.
    pub sub_issue_count: u64,
    /// How many of them are closed.
    #[serde(default)]
    pub sub_issues_completed: u64,
    /// GitHub's native "blocked by" relations.
    pub native_blockers: Vec<IssueRef>,
    /// References under the `Dependencies / Blockers` heading of the body.
    pub body_blockers: Vec<IssueRef>,
}

impl Issue {
    pub fn r(&self) -> &IssueRef {
        &self.status.r
    }

    /// Native and body blockers, without duplicates. Neither the issue itself
    /// nor its parent counts: a parent closes only after its sub-issues, so a
    /// sub-issue waiting on it would wait forever. Bodies mention the parent
    /// for context ("None beyond #575's landed slices").
    pub fn blockers(&self) -> Vec<IssueRef> {
        let mut out: Vec<IssueRef> = Vec::new();
        let parent = self.parent.as_ref().map(IssueRef::key);
        for b in self.native_blockers.iter().chain(&self.body_blockers) {
            if b.key() != self.r().key()
                && Some(b.key()) != parent
                && !out.iter().any(|o| o.key() == b.key())
            {
                out.push(b.clone());
            }
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Epic {
    pub status: IssueStatus,
    /// In the epic's own order.
    pub sub_issues: Vec<IssueStatus>,
}

/// An open pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub r: IssueRef,
    pub head_ref: String,
    pub head_sha: String,
    pub draft: bool,
}

/// An open pull request that carries `dispatch:review` or
/// `dispatch:approved` itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabeledPr {
    /// The PR's number, title and labels, as for an issue.
    pub status: IssueStatus,
    pub pr: PullRequest,
    /// The open issues of its repository it closes (`Closes #N`).
    pub closes: Vec<IssueRef>,
    /// From a branch of its own repository into the default branch, the
    /// only PRs issuewright reviews and merges.
    pub own_into_default: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Open issues with a `dispatch:*` label in the watched repositories.
    pub issues: Vec<Issue>,
    /// Parents of those issues, keyed by `IssueRef::key`.
    pub epics: BTreeMap<(String, u64), Epic>,
    /// Status of every other issue the scheduler needs (blockers).
    pub others: BTreeMap<(String, u64), IssueStatus>,
    /// Blockers that couldn't be resolved, with the reason.
    pub unresolved: BTreeMap<(String, u64), String>,
    /// Open pull requests labeled `dispatch:review` or `dispatch:approved`.
    #[serde(default)]
    pub prs: Vec<LabeledPr>,
}

impl Snapshot {
    /// The labels' holder a session is keyed on: a `dispatch:*` issue, or a
    /// labeled pull request reviewed on its own.
    pub fn labeled(&self, r: &IssueRef) -> Option<&IssueStatus> {
        let k = r.key();
        self.issues
            .iter()
            .map(|i| &i.status)
            .chain(self.prs.iter().map(|p| &p.status))
            .find(|s| s.r.key() == k)
    }

    /// Looks an issue's status up wherever the snapshot has it.
    pub fn status(&self, r: &IssueRef) -> Option<&IssueStatus> {
        let k = r.key();
        if let Some(s) = self.others.get(&k) {
            return Some(s);
        }
        if let Some(i) = self.issues.iter().find(|i| i.r().key() == k) {
            return Some(&i.status);
        }
        if let Some(e) = self.epics.get(&k) {
            return Some(&e.status);
        }
        self.epics
            .values()
            .flat_map(|e| e.sub_issues.iter())
            .find(|s| s.r.key() == k)
    }
}
