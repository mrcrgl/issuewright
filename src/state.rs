//! issuewright's local state under `<home>/state`: slot locks, history, one-time
//! comments, the last plan, and the single-instance lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::model::IssueRef;
use crate::scheduler::{History, Plan};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Interactive,
    Unattended,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Interactive => "interactive",
            Mode::Unattended => "unattended",
        })
    }
}

/// What a session does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Works on the issue and opens a PR.
    #[default]
    Implement,
    /// Reviews the issue's PR and sets a verdict label (`auto_merge`).
    Review,
    /// Checks an epic whose sub-issues are all closed against its
    /// acceptance criteria, and closes it when they are met.
    EpicCheck,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Implement => "implement",
            Kind::Review => "review",
            Kind::EpicCheck => "epic-check",
        })
    }
}

/// The PR a reviewer session reviews, pinned to the commit it saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedPr {
    pub pr: IssueRef,
    pub head_sha: String,
}

/// One running session, holding one slot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotLock {
    pub repo: String,
    pub slot: usize,
    pub issue: IssueRef,
    pub mode: Mode,
    pub branch: String,
    /// Session and window name, also the Remote Control name.
    pub name: String,
    pub tmux_window: Option<String>,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub kind: Kind,
    /// Review sessions only.
    #[serde(default)]
    pub review: Option<ReviewedPr>,
    /// When issuewright first saw the session's issue at its end label; the window
    /// is closed `close_done_after_secs` later.
    #[serde(default)]
    pub done_since: Option<DateTime<Utc>>,
    /// The Claude Code session id (`--session-id`), to resume it later.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// What issuewright keeps to resume an issue's conversation: when its PR's CI
/// fails or conflicts, and when its question was answered on GitHub.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CiWatch {
    /// The last implementing session of each issue (keyed like
    /// [`History::key`]), resumed when CI fails.
    pub sessions: BTreeMap<String, Conversation>,
    /// When issuewright first saw each issue at `dispatch:ci` with its PR at
    /// this head commit.
    pub seen: BTreeMap<String, CiSeen>,
    /// Issues whose session moved its unanswered question to GitHub; the
    /// next claim resumes that conversation.
    #[serde(default)]
    pub asked: BTreeMap<String, Asked>,
    /// How often issuewright resumed each issue's session because its
    /// reviewer requested changes, since it last went to you.
    #[serde(default)]
    pub review_rounds: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asked {
    pub at: DateTime<Utc>,
    /// The branch the session pushed its work in progress to.
    pub branch: String,
}

/// A session at the end of a turn, waiting for someone to answer it. Its
/// `Stop` hook writes this, its `UserPromptSubmit` hook removes it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Waiting {
    pub since: DateTime<Utc>,
    /// When issuewright told it to move its question to GitHub.
    #[serde(default)]
    pub nudged_at: Option<DateTime<Utc>>,
}

/// Sessions running at one time, the most within each ten minutes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub at: DateTime<Utc>,
    pub busy: usize,
}

/// The backlog at one poll, the last of each hour.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BacklogSample {
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub counts: crate::graph::Backlog,
}

/// How long the backlog's history goes back.
const BACKLOG_DAYS: i64 = 90;

/// Adds `counts` at `now` to `history`, replacing the sample of the same
/// hour, and drops samples older than [`BACKLOG_DAYS`].
pub fn add_backlog(
    history: &mut Vec<BacklogSample>,
    now: DateTime<Utc>,
    counts: crate::graph::Backlog,
) {
    let hour = |t: DateTime<Utc>| t.timestamp().div_euclid(3600);
    match history.last_mut() {
        Some(s) if hour(s.at) == hour(now) => *s = BacklogSample { at: now, counts },
        _ => history.push(BacklogSample { at: now, counts }),
    }
    let since = now - chrono::Duration::days(BACKLOG_DAYS);
    history.retain(|s| s.at > since);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiSeen {
    pub head_sha: String,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub session_id: String,
    pub slot: usize,
    /// The slot's directory, where Claude Code keeps the transcript.
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub last_poll: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub mode: Option<Mode>,
    pub plan: Plan,
    /// Issues of running sessions that already reached their end label.
    pub done_waiting_to_close: Vec<IssueRef>,
    /// The `dispatch:*` issues and their blockers.
    #[serde(default)]
    pub graph: Vec<crate::graph::Node>,
    /// The last day's slot usage, oldest first.
    #[serde(default)]
    pub usage: Vec<Usage>,
    /// Errors that have repeated poll after poll, oldest first.
    #[serde(default)]
    pub alarms: Vec<crate::health::Alarm>,
    /// Why new sessions wait for resources, if they do.
    #[serde(default)]
    pub hold: Option<String>,
}

impl Status {
    /// Adds `busy` sessions at `now` to the usage, in ten-minute buckets, and
    /// drops samples older than a day.
    pub fn record_usage(&mut self, now: DateTime<Utc>, busy: usize) {
        const BUCKET: i64 = 600;
        let bucket = |t: DateTime<Utc>| t.timestamp().div_euclid(BUCKET);
        match self.usage.last_mut() {
            Some(u) if bucket(u.at) == bucket(now) => u.busy = u.busy.max(busy),
            _ => self.usage.push(Usage { at: now, busy }),
        }
        let since = now - chrono::Duration::days(1);
        self.usage.retain(|u| u.at > since);
    }
}

pub struct State {
    dir: PathBuf,
}

fn read_json<T: DeserializeOwned + Default>(p: &Path) -> Result<T> {
    match fs::read_to_string(p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// Writes through a temporary file so a crash never leaves half a file.
fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    fs::rename(&tmp, p)?;
    Ok(())
}

impl State {
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(dir.join("locks"))?;
        fs::create_dir_all(dir.join("sessions"))?;
        fs::create_dir_all(dir.join("waiting"))?;
        Ok(Self { dir })
    }

    /// Holds an exclusive lock for the lifetime of the returned file, so only
    /// one issuewright runs per home.
    pub fn instance_lock(&self) -> Result<File> {
        let f = File::create(self.dir.join("issuewright.lock"))?;
        if f.try_lock().is_err() {
            bail!(
                "another issuewright is running with state in {}",
                self.dir.display()
            );
        }
        Ok(f)
    }

    fn lock_path(&self, repo: &str, slot: usize) -> PathBuf {
        self.dir
            .join("locks")
            .join(format!("{}-{slot}.json", repo.replace('/', "__")))
    }

    pub fn locks(&self) -> Result<Vec<SlotLock>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("locks"))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") {
                let s = fs::read_to_string(&p)?;
                out.push(
                    serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?,
                );
            }
        }
        out.sort_by(|a: &SlotLock, b| (&a.repo, a.slot).cmp(&(&b.repo, b.slot)));
        Ok(out)
    }

    pub fn write_lock(&self, l: &SlotLock) -> Result<()> {
        write_json(&self.lock_path(&l.repo, l.slot), l)
    }

    pub fn remove_lock(&self, l: &SlotLock) -> Result<()> {
        self.clear_waiting(&l.repo, l.slot)?;
        match fs::remove_file(self.lock_path(&l.repo, l.slot)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn waiting_path(&self, repo: &str, slot: usize) -> PathBuf {
        self.dir
            .join("waiting")
            .join(format!("{}-{slot}.json", repo.replace('/', "__")))
    }

    pub fn waiting(&self, repo: &str, slot: usize) -> Option<Waiting> {
        read_json::<Option<Waiting>>(&self.waiting_path(repo, slot))
            .ok()
            .flatten()
    }

    pub fn set_waiting(&self, repo: &str, slot: usize, w: &Waiting) -> Result<()> {
        write_json(&self.waiting_path(repo, slot), w)
    }

    pub fn clear_waiting(&self, repo: &str, slot: usize) -> Result<()> {
        match fs::remove_file(self.waiting_path(repo, slot)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn session_dir(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    pub fn history(&self) -> Result<History> {
        read_json(&self.dir.join("history.json"))
    }
    pub fn save_history(&self, h: &History) -> Result<()> {
        write_json(&self.dir.join("history.json"), h)
    }

    /// Keys of one-time comments already posted.
    pub fn commented(&self) -> Result<BTreeSet<String>> {
        read_json(&self.dir.join("commented.json"))
    }
    pub fn save_commented(&self, c: &BTreeSet<String>) -> Result<()> {
        write_json(&self.dir.join("commented.json"), c)
    }

    pub fn ci_watch(&self) -> Result<CiWatch> {
        read_json(&self.dir.join("ci.json"))
    }
    pub fn save_ci_watch(&self, w: &CiWatch) -> Result<()> {
        write_json(&self.dir.join("ci.json"), w)
    }

    pub fn status(&self) -> Result<Status> {
        read_json(&self.dir.join("status.json"))
    }
    pub fn save_status(&self, s: &Status) -> Result<()> {
        write_json(&self.dir.join("status.json"), s)
    }

    /// The backlog at each hour of the last [`BACKLOG_DAYS`] days, oldest first.
    pub fn backlog(&self) -> Result<Vec<BacklogSample>> {
        read_json(&self.dir.join("backlog.json"))
    }
    pub fn record_backlog(&self, now: DateTime<Utc>, counts: crate::graph::Backlog) -> Result<()> {
        let mut h = self.backlog()?;
        add_backlog(&mut h, now, counts);
        write_json(&self.dir.join("backlog.json"), &h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_keeps_the_peak_of_each_ten_minutes_for_a_day() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let mut st = Status::default();
        st.record_usage(t("2026-11-22T10:00:00Z"), 1);
        st.record_usage(t("2026-11-22T10:05:00Z"), 3);
        st.record_usage(t("2026-11-22T10:09:00Z"), 2);
        st.record_usage(t("2026-11-22T10:10:00Z"), 0);
        assert_eq!(st.usage.iter().map(|u| u.busy).collect::<Vec<_>>(), [3, 0]);
        st.record_usage(t("2026-11-23T10:05:00Z"), 1);
        assert_eq!(st.usage.iter().map(|u| u.busy).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn backlog_keeps_the_last_sample_of_each_hour() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let b = |q| crate::graph::Backlog {
            qualified: q,
            ..Default::default()
        };
        let mut h = Vec::new();
        add_backlog(&mut h, t("2026-08-01T10:00:00Z"), b(9));
        add_backlog(&mut h, t("2026-11-22T10:05:00Z"), b(1));
        add_backlog(&mut h, t("2026-11-22T10:50:00Z"), b(2));
        add_backlog(&mut h, t("2026-11-22T11:01:00Z"), b(3));
        let q: Vec<usize> = h.iter().map(|s| s.counts.qualified).collect();
        assert_eq!(q, [2, 3]);
        let json = serde_json::to_string(&h[0]).unwrap();
        assert!(json.contains(r#""qualified":2"#), "{json}");
    }
}
