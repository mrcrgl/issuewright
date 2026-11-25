//! The poll loop: snapshot, end detection, scheduling, claims.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::app::ReviewApp;
use crate::config::{Config, MergeMethod, RepoConfig};
use crate::github::{Checks, Gh, IssueBrief, MergeGates, Merged, fetch_snapshot};
use crate::health::{self, Alarms, Backoff, Event};
use crate::model::*;
use crate::scheduler::{Capacity, Entry, History, Plan, Policy, Verdict, schedule};
use crate::session;
use crate::slots::{self, Slot};
use crate::state::{Asked, CiSeen, Conversation, Kind, Mode, ReviewedPr, SlotLock, State, Status};
use crate::tmux;

pub struct Options {
    /// The config file, absolute; sessions' `Stop` hooks read it.
    pub config: PathBuf,
    pub mode: Mode,
    pub dry_run: bool,
    pub once: bool,
}

pub struct Daemon {
    cfg: Config,
    opts: Options,
    gh: Gh,
    state: State,
    claude: PathBuf,
    /// This binary, for the sessions' `Stop` hook.
    exe: PathBuf,
    log_path: PathBuf,
    stop: Arc<AtomicBool>,
    /// The GitHub App reviewer sessions act as; reviews are off without it.
    app: Option<ReviewApp>,
    /// `auto_merge` repositories the app is installed on (lowercase), as of
    /// the start. Only these get reviews.
    review_repos: BTreeSet<String>,
    /// Epics seen closed, so later polls don't look them up again.
    closed_epics: BTreeSet<(String, u64)>,
    /// The config file as last loaded, and its modification time, to notice
    /// edits.
    config_text: String,
    config_mtime: Option<std::time::SystemTime>,
    /// The errors logged in the current poll, for the alarms.
    errors: RefCell<Vec<String>>,
    alarms: Alarms,
    /// Issues whose session failed to start, or whose CI or merge check
    /// failed, wait a little longer after each failure.
    backoff: RefCell<Backoff<(String, u64)>>,
    /// Why new sessions wait for resources, if they do.
    hold: Option<String>,
    /// Whether that is only the load: a P0 issue still starts then.
    hold_is_load: bool,
}

pub fn log_line(path: &std::path::Path, msg: &str) {
    let line = format!("{} {msg}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
    eprintln!("{line}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn which(bin: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("{bin} not found on PATH")
}

/// The priority that goes first: P0.
const URGENT: u8 = 0;

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // Signal 0 only checks that the process exists. EPERM means it does,
    // but belongs to someone else.
    // SAFETY: kill with signal 0 sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

impl Daemon {
    pub fn new(cfg: Config, opts: Options, stop: Arc<AtomicBool>) -> Result<Self> {
        if opts.mode == Mode::Unattended && !opts.dry_run {
            bail!("unattended mode is not implemented yet; use --mode interactive");
        }
        std::fs::create_dir_all(cfg.logs_dir())?;
        let state = State::open(cfg.state_dir())?;
        let gh = Gh::new(cfg.gh_user.as_deref())?;
        let claude = which("claude")?;
        let exe = crate::hook::current_exe()?;
        let log_path = cfg.logs_dir().join("issuewright.log");
        let app = match &cfg.review_app {
            Some(a) if !opts.dry_run => Some(ReviewApp::load(a)?),
            _ => None,
        };
        let config_mtime = mtime(&opts.config);
        let config_text = std::fs::read_to_string(&opts.config).unwrap_or_default();
        Ok(Self {
            cfg,
            opts,
            gh,
            state,
            claude,
            exe,
            log_path,
            stop,
            app,
            review_repos: BTreeSet::new(),
            closed_epics: BTreeSet::new(),
            config_text,
            config_mtime,
            errors: RefCell::default(),
            alarms: Alarms::default(),
            backoff: RefCell::default(),
            hold: None,
            hold_is_load: false,
        })
    }

    fn log(&self, msg: &str) {
        log_line(&self.log_path, msg);
    }

    /// Logs an error and counts it toward the alarms.
    fn error(&self, msg: &str) {
        self.log(msg);
        self.errors.borrow_mut().push(msg.to_string());
    }

    /// Whether `r` still waits after failed attempts.
    fn backing_off(&self, r: &IssueRef) -> bool {
        self.backoff
            .borrow()
            .waiting(&r.key(), Utc::now())
            .is_some()
    }

    /// Records how an attempt for `r` went; a failure is logged as `what`.
    fn attempted<T>(&self, r: &IssueRef, res: Result<T>, what: &str) -> Option<T> {
        match res {
            Ok(v) => {
                self.backoff.borrow_mut().ok(&r.key());
                Some(v)
            }
            Err(e) => {
                self.error(&format!("{r}: {what} failed: {e:#}"));
                self.backoff.borrow_mut().fail(&r.key(), Utc::now());
                None
            }
        }
    }

    /// Takes the poll's errors: raises an alarm for one that has repeated
    /// for `alarm_after_polls` polls, clears it once it stops, and puts the
    /// raised ones in `status.json`.
    fn raise_alarms(&mut self) {
        let errors = std::mem::take(&mut *self.errors.borrow_mut());
        let now = Utc::now();
        for ev in self
            .alarms
            .observe(&errors, now, self.cfg.alarm_after_polls)
        {
            let msg = match ev {
                Event::Raised(a) => format!("alarm: {}", a.summary(now)),
                Event::Cleared(a) => format!(
                    "alarm cleared after {} min: {}",
                    (now - a.since).num_minutes(),
                    a.message
                ),
            };
            self.log(&msg);
            if let Err(e) = health::notify(&self.cfg.alarm_command, &msg) {
                self.log(&format!("alarm_command failed: {e}"));
            }
        }
        if let Ok(mut st) = self.state.status() {
            st.alarms = self.alarms.active();
            st.hold = self.hold.clone();
            let _ = self.state.save_status(&st);
        }
    }

    /// Holds new sessions while a disk is nearly full or the host is
    /// overloaded, and logs when that starts and ends.
    fn check_resources(&mut self) {
        let paths: Vec<PathBuf> = std::iter::once(self.cfg.home.clone())
            .chain(self.cfg.disk_paths.iter().cloned())
            .collect();
        let disk = health::disk_hold(&paths, self.cfg.min_free_disk_gb);
        self.hold_is_load = disk.is_none();
        let hold = disk
            .or_else(|| health::load_hold(self.cfg.max_load))
            .map(|why| format!("new sessions wait: {why}"));
        match (&self.hold, &hold) {
            (None, Some(h)) => self.log(h),
            (Some(_), None) => self.log("new sessions start again: resources are back"),
            _ => {}
        }
        if let Some(h) = &hold {
            self.errors.borrow_mut().push(h.clone());
        }
        self.hold = hold;
    }

    fn repos(&self) -> Vec<String> {
        self.cfg.repos.iter().map(|r| r.name.clone()).collect()
    }

    fn policy(&self) -> Policy {
        Policy {
            blocker_done: self.cfg.blocker_done,
            max_started_epics: self.cfg.max_started_epics,
            max_sessions: self.cfg.max_sessions,
            max_in_ci: self
                .cfg
                .repos
                .iter()
                .filter_map(|r| Some((r.name.to_lowercase(), r.max_in_ci?)))
                .collect(),
            hold: self.hold.clone(),
            only_priority: None,
            held: self
                .backoff
                .borrow()
                .all_waiting(Utc::now())
                .into_iter()
                .map(|(k, n, at)| {
                    (
                        k,
                        format!(
                            "failed {n} times, next try at {}",
                            at.with_timezone(&chrono::Local).format("%H:%M")
                        ),
                    )
                })
                .collect(),
        }
    }

    /// A lock's session is alive while its tmux window exists and the process
    /// in it runs. `None` when tmux can't be asked.
    fn alive(
        lock: &SlotLock,
        windows: Option<&std::collections::BTreeSet<String>>,
    ) -> Option<bool> {
        match (&lock.tmux_window, windows) {
            (Some(w), Some(ws)) => Some(ws.contains(w) && lock.pid.is_none_or(pid_alive)),
            (Some(_), None) => None,
            (None, _) => Some(lock.pid.is_some_and(pid_alive)),
        }
    }

    fn capacity(&self, locks: &[SlotLock]) -> Capacity {
        let mut free = BTreeMap::new();
        for r in &self.cfg.repos {
            let used = locks
                .iter()
                .filter(|l| l.repo.eq_ignore_ascii_case(&r.name))
                .count();
            free.insert(r.name.to_lowercase(), r.slots.saturating_sub(used));
        }
        Capacity {
            running: locks.len(),
            free_slots: free,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let _instance = if self.opts.dry_run {
            None
        } else {
            Some(self.state.instance_lock()?)
        };
        if self.opts.dry_run {
            let snap = fetch_snapshot(&self.gh, &self.repos())?;
            let locks = self.state.locks()?;
            let plan = schedule(
                &snap,
                &self.policy(),
                &self.capacity(&locks),
                &self.state.history()?,
            );
            print_plan(&plan, &locks);
            return Ok(());
        }
        if self.opts.mode == Mode::Interactive {
            tmux::ensure_session(&self.cfg.tmux_session, &self.log_path)?;
        }
        for r in &self.cfg.repos {
            self.gh
                .ensure_labels(&r.name)
                .with_context(|| format!("creating labels in {}", r.name))?;
        }
        self.log(&format!(
            "issuewright started: mode {}, {} repos, max_sessions {}",
            self.opts.mode,
            self.cfg.repos.len(),
            self.cfg.max_sessions
        ));
        self.check_auto_merge();
        self.reconcile()?;

        let mut started_here: Vec<SlotLock> = Vec::new();
        loop {
            let t0 = Instant::now();
            self.reload_config();
            self.check_resources();
            match self.cycle() {
                Ok(claimed) => started_here.extend(claimed),
                Err(e) => {
                    self.error(&format!("poll failed: {e:#}"));
                    let mut st = self.state.status().unwrap_or_default();
                    st.last_error = Some(format!("{e:#}"));
                    let _ = self.state.save_status(&st);
                }
            }
            self.raise_alarms();
            if self.opts.once {
                break;
            }
            if self.sleep(
                Duration::from_secs(self.cfg.poll_interval_secs).saturating_sub(t0.elapsed()),
            ) {
                break;
            }
        }
        if self.opts.once && !started_here.is_empty() {
            self.log(&format!(
                "--once: waiting for {} session(s) to end",
                started_here.len()
            ));
            loop {
                let windows = tmux::window_ids();
                let running = started_here
                    .iter()
                    .filter(|l| Self::alive(l, windows.as_ref()) != Some(false))
                    .count();
                if running == 0 || self.sleep(Duration::from_secs(5)) {
                    break;
                }
            }
            let snap = fetch_snapshot(&self.gh, &self.repos())?;
            self.handle_ended(&snap)?;
        }
        if self.stop.load(Ordering::SeqCst) {
            self.log("stopping: no new claims; interactive windows stay open");
        }
        Ok(())
    }

    /// Applies an edited config file before the next poll. A file that doesn't
    /// load keeps the previous config, and so does one that removes a
    /// repository while a session of it holds a slot. `home`, `tmux_session`
    /// and `gh_user` change only on restart.
    fn reload_config(&mut self) {
        let m = mtime(&self.opts.config);
        if m.is_none() || m == self.config_mtime {
            return;
        }
        self.config_mtime = m;
        let not = |me: &Self, why: String| {
            me.log(&format!(
                "config not reloaded, the previous one stays: {why}"
            ))
        };
        let text = match std::fs::read_to_string(&self.opts.config) {
            Ok(t) => t,
            Err(e) => return not(self, e.to_string()),
        };
        let mut new = match Config::load(&self.opts.config) {
            Ok(c) => c,
            Err(e) => return not(self, format!("{e:#}")),
        };
        let changed = crate::config::changed_keys(&self.config_text, &text).unwrap_or_default();
        if changed.is_empty() {
            self.config_text = text;
            return;
        }
        let locks = self.state.locks().unwrap_or_default();
        let busy: Vec<&str> = self
            .cfg
            .repos
            .iter()
            .filter(|r| new.repo(&r.name).is_none())
            .filter(|r| locks.iter().any(|l| l.repo.eq_ignore_ascii_case(&r.name)))
            .map(|r| r.name.as_str())
            .collect();
        if !busy.is_empty() {
            return not(
                self,
                format!(
                    "it removes {}, which still has a session in a slot; remove it once the slot is free",
                    busy.join(", ")
                ),
            );
        }
        if format!("{:?}", new.review_app) != format!("{:?}", self.cfg.review_app) {
            self.app = match &new.review_app {
                Some(a) => match ReviewApp::load(a) {
                    Ok(app) => Some(app),
                    Err(e) => return not(self, format!("review_app: {e:#}")),
                },
                None => None,
            };
        }
        let mut on_restart = Vec::new();
        if new.home != self.cfg.home {
            new.home = self.cfg.home.clone();
            on_restart.push("home");
        }
        if new.tmux_session != self.cfg.tmux_session {
            new.tmux_session = self.cfg.tmux_session.clone();
            on_restart.push("tmux_session");
        }
        if new.gh_user != self.cfg.gh_user {
            new.gh_user = self.cfg.gh_user.clone();
            on_restart.push("gh_user");
        }
        for r in &new.repos {
            if self.cfg.repo(&r.name).is_none()
                && let Err(e) = self.gh.ensure_labels(&r.name)
            {
                self.log(&format!("creating labels in {}: {e:#}", r.name));
            }
        }
        self.cfg = new;
        self.config_text = text;
        let mut msg = format!("config reloaded: {}", changed.join(", "));
        if !on_restart.is_empty() {
            msg += &format!(" ({} take effect on restart)", on_restart.join(", "));
        }
        self.log(&msg);
        if changed
            .iter()
            .any(|k| k.starts_with("repo") || k.starts_with("review_app"))
        {
            self.review_repos.clear();
            self.check_auto_merge();
        }
    }

    /// Sleeps in small steps; true when a stop signal arrived.
    fn sleep(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.stop.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        self.stop.load(Ordering::SeqCst)
    }

    /// At start: drop locks whose session is gone, and hand orphaned
    /// `dispatch:running` issues back to you.
    fn reconcile(&self) -> Result<()> {
        let snap = fetch_snapshot(&self.gh, &self.repos())?;
        self.handle_ended(&snap)?;
        let locks = self.state.locks()?;
        for i in &snap.issues {
            if !i.status.has_label(RUNNING) || locks.iter().any(|l| l.issue.key() == i.r().key()) {
                continue;
            }
            self.log(&format!(
                "{}: dispatch:running without a live session, needs input",
                i.r()
            ));
            self.gh.edit_labels(i.r(), &[NEEDS_INPUT], &[RUNNING])?;
            self.gh.comment(
                i.r(),
                "issuewright found this issue `dispatch:running` at start, but no session for it is alive \
                 (issuewright was restarted and the session is gone). Set `dispatch:ready` again to start a fresh session.",
            )?;
        }
        Ok(())
    }

    /// Frees the slots of sessions that ended. An issue still
    /// `dispatch:running` then goes to `dispatch:needs-input`.
    fn handle_ended(&self, snap: &Snapshot) -> Result<()> {
        let windows = tmux::window_ids();
        for lock in self.state.locks()? {
            if Self::alive(&lock, windows.as_ref()) != Some(false) {
                continue;
            }
            let issue = snap.issues.iter().find(|i| i.r().key() == lock.issue.key());
            if lock.kind == Kind::Review {
                self.review_ended(&lock)?;
            } else if let Some(i) = issue.filter(|i| {
                // The snapshot may predate the session's last label change;
                // it exits right after it.
                i.status.has_label(RUNNING) && self.still_running(i.r())
            }) {
                self.log(&format!(
                    "{}: session ended without an end label, needs input",
                    lock.name
                ));
                self.gh.edit_labels(i.r(), &[NEEDS_INPUT], &[RUNNING])?;
                self.gh.comment(
                    i.r(),
                    &format!(
                        "The issuewright session `{}` (slot {} of {}, branch `{}`) ended while the issue was still \
                         `dispatch:running`, so it didn't finish with a PR, a question, a split or a closed epic. \
                         Look at the branch and set `dispatch:ready` again to start a fresh session.",
                        lock.name, lock.slot, lock.repo, lock.branch
                    ),
                )?;
            } else {
                self.log(&format!(
                    "{}: session ended, slot {} of {} is free",
                    lock.name, lock.slot, lock.repo
                ));
            }
            self.state.remove_lock(&lock)?;
        }
        Ok(())
    }

    /// A reviewer session ended: merge on approval, hand back without a
    /// verdict. Judged from the subject's current labels (the issue, or a PR
    /// reviewed on its own), not the poll's snapshot: a session exits right
    /// after setting its verdict, which may postdate the snapshot.
    fn review_ended(&self, lock: &SlotLock) -> Result<()> {
        let subject = &lock.issue;
        match self.gh.state_and_labels(subject)? {
            Some((_, labels)) if has_label(&labels, APPROVED) => self.merge(lock, subject)?,
            Some((open, labels)) if awaits_review(open, &labels) => {
                self.log(&format!(
                    "{}: review ended without a verdict, needs input",
                    lock.name
                ));
                self.gh.edit_labels(subject, &[NEEDS_INPUT], &[REVIEW])?;
                self.gh.comment(
                    subject,
                    &format!(
                        "The issuewright reviewer session `{}` ended without a verdict, so nothing was merged. \
                         Review the PR yourself, or replace `{NEEDS_INPUT}` with `{REVIEW}` for a fresh review.",
                        lock.name
                    ),
                )?;
            }
            _ => self.log(&format!(
                "{}: review ended, slot {} of {} is free",
                lock.name, lock.slot, lock.repo
            )),
        }
        Ok(())
    }

    /// Moves `dispatch:review` from pull requests to the issues they close:
    /// issuewright reviews an issue together with the PR that closes it, but
    /// agents outside issuewright (and sometimes its own sessions) label the PR. A
    /// PR that closes no issue keeps the label and is reviewed on its own.
    fn move_review_labels_to_issues(&self, snap: &Snapshot) -> Result<()> {
        let locks = self.state.locks()?;
        for pr in &snap.prs {
            if pr.closes.is_empty()
                || !pr.status.has_label(REVIEW)
                || locks.iter().any(|l| l.issue.key() == pr.status.r.key())
            {
                continue;
            }
            if let Err(e) = self.move_review_label(pr) {
                self.error(&format!(
                    "{}: moving {REVIEW} to its issue failed: {e:#}",
                    pr.status.r
                ));
            }
        }
        Ok(())
    }

    fn move_review_label(&self, pr: &LabeledPr) -> Result<()> {
        // Label the issues first: if that fails, the PR keeps the label and
        // the next poll tries again.
        for issue in &pr.closes {
            self.gh.edit_labels(issue, &[REVIEW], &[])?;
        }
        self.gh.remove_label(&pr.status.r, REVIEW)?;
        let list = pr
            .closes
            .iter()
            .map(|i| format!("#{}", i.number))
            .collect::<Vec<_>>()
            .join(", ");
        self.log(&format!("{}: moved {REVIEW} to {list}", pr.status.r));
        self.gh.comment(
            &pr.status.r,
            &format!(
                "This PR closes {list}, so issuewright moved `{REVIEW}` there: it reviews an issue together with the PR \
                 that closes it. Next time, set it on the issue. Only a PR that closes no issue is reviewed with \
                 the label on the PR itself."
            ),
        )
    }

    fn cycle(&mut self) -> Result<Vec<SlotLock>> {
        let mut snap = fetch_snapshot(&self.gh, &self.repos())?;
        self.move_review_labels_to_issues(&snap)?;
        if self.opts.mode == Mode::Interactive {
            self.close_done(&snap)?;
        }
        self.handle_ended(&snap)?;
        self.merge_approved(&snap)?;
        let mut claimed = Vec::new();
        let mut hist = self.state.history()?;
        if !self.stop.load(Ordering::SeqCst) {
            claimed = self.claim_urgent(&mut snap, &mut hist)?;
            self.check_ci(&snap)?;
            self.check_merging(&snap)?;
            self.check_questions()?;
            self.start_reviews(&snap)?;
            if self.cfg.check_epics {
                self.start_epic_checks(&snap)?;
            }
        }
        let locks = self.state.locks()?;
        let plan = if self.stop.load(Ordering::SeqCst) {
            Plan::default()
        } else {
            schedule(&snap, &self.policy(), &self.capacity(&locks), &hist)
        };
        self.comment_unresolvable(&plan)?;
        self.comment_epics_without_sub_issues(&snap)?;

        let mut plan_out = plan.clone();
        for entry in plan.claims() {
            let res = self.claim(entry, &snap, &mut hist);
            let why = res.as_ref().err().map(|e| format!("claim failed: {e:#}"));
            match self.attempted(&entry.issue, res, "claim") {
                Some(lock) => claimed.push(lock),
                None => {
                    if let Some(x) = plan_out.entries.iter_mut().find(|x| x.issue == entry.issue) {
                        x.verdict = Verdict::Waiting(why.unwrap_or_default());
                    }
                }
            }
        }
        let done_waiting = self
            .state
            .locks()?
            .iter()
            .filter(|l| session_done(l, &snap))
            .map(|l| l.issue.clone())
            .collect();
        let mut status = Status {
            last_poll: Some(Utc::now()),
            last_error: None,
            mode: Some(self.opts.mode),
            plan: plan_out,
            done_waiting_to_close: done_waiting,
            graph: crate::graph::build(&snap, &self.policy()),
            usage: self.state.status().map(|s| s.usage).unwrap_or_default(),
            alarms: self.alarms.active(),
            hold: self.hold.clone(),
        };
        status.record_usage(Utc::now(), self.state.locks()?.len());
        self.state.save_status(&status)?;
        Ok(claimed)
    }

    /// Claims ready P0 issues before CI fixes, rebases and reviews take the
    /// free slots, past `max_in_ci` and a hold for load (not one for disk).
    /// The issues claimed are marked running in `snap`, so the rest of the
    /// poll treats them as such.
    fn claim_urgent(&self, snap: &mut Snapshot, hist: &mut History) -> Result<Vec<SlotLock>> {
        let urgent = snap.issues.iter().any(|i| {
            i.status.state == IssueState::Open
                && i.status.has_label(READY)
                && i.status.priority() == URGENT
        });
        if !urgent {
            return Ok(Vec::new());
        }
        let policy = Policy {
            only_priority: Some(URGENT),
            max_in_ci: BTreeMap::new(),
            hold: if self.hold_is_load {
                None
            } else {
                self.hold.clone()
            },
            ..self.policy()
        };
        let plan = schedule(snap, &policy, &self.capacity(&self.state.locks()?), hist);
        let mut claimed = Vec::new();
        for entry in plan.claims() {
            let res = self.claim(entry, snap, hist);
            if let Some(lock) = self.attempted(&entry.issue, res, "claim") {
                claimed.push(lock);
                if let Some(i) = snap
                    .issues
                    .iter_mut()
                    .find(|i| i.r().key() == entry.issue.key())
                {
                    i.status.labels.retain(|l| l != READY);
                    i.status.labels.push(RUNNING.to_string());
                }
            }
        }
        Ok(claimed)
    }

    /// A released `type:epic` issue without sub-issues is skipped; say once
    /// why, and how to get its parts scheduled.
    fn comment_epics_without_sub_issues(&self, snap: &Snapshot) -> Result<()> {
        for i in &snap.issues {
            let s = &i.status;
            if s.has_label(READY) && s.has_label(TYPE_EPIC) && i.sub_issue_count == 0 {
                self.comment_once(
                    i.r(),
                    &format!("epic-without-sub-issues:{}", i.r()),
                    &format!(
                        "issuewright doesn't start sessions for epics, and this one has no sub-issues on GitHub, so \
                         nothing it lists is scheduled as part of it. Link its parts as sub-issues (\"Add sub-issue\" \
                         on this page, in the epic's order), then release them with `{READY}`. `{READY}` on the \
                         epic itself has no effect."
                    ),
                )?;
            }
        }
        Ok(())
    }

    fn comment_unresolvable(&self, plan: &Plan) -> Result<()> {
        if plan.unresolvable.is_empty() {
            return Ok(());
        }
        let mut done = self.state.commented()?;
        for u in &plan.unresolvable {
            let key = format!("unresolvable:{}:{}", u.dependent, u.blocker);
            if done.contains(&key) {
                continue;
            }
            self.gh.comment(
                &u.dependent,
                &format!(
                    "issuewright can't resolve the blocker {} ({}), so this issue waits. Fix or remove the reference \
                     (native dependency or under `Dependencies / Blockers`) to release it.",
                    u.blocker, u.reason
                ),
            )?;
            done.insert(key);
            self.state.save_commented(&done)?;
        }
        Ok(())
    }

    /// The token the sessions get: `session.gh_token_command`, or issuewright's own.
    fn session_token(&self) -> Result<String> {
        let Some(cmd) = &self.cfg.session.gh_token_command else {
            return Ok(self.gh.token().to_string());
        };
        let out = std::process::Command::new("sh")
            .args(["-c", cmd])
            .output()?;
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || t.is_empty() {
            bail!(
                "session.gh_token_command failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(t)
    }

    /// The lowest free slot of `repo`.
    fn free_slot(&self, repo: &RepoConfig) -> Result<usize> {
        let locks = self.state.locks()?;
        (1..=repo.slots)
            .find(|i| {
                !locks
                    .iter()
                    .any(|l| l.repo.eq_ignore_ascii_case(&repo.name) && l.slot == *i)
            })
            .context("no free slot")
    }

    /// Whether another session fits, overall and in `repo`.
    fn has_capacity(&self, repo: &RepoConfig) -> Result<bool> {
        if self.hold.is_some() {
            return Ok(false);
        }
        let locks = self.state.locks()?;
        let cap = self.capacity(&locks);
        Ok(cap.running < self.cfg.max_sessions
            && cap
                .free_slots
                .get(&repo.name.to_lowercase())
                .copied()
                .unwrap_or(0)
                > 0)
    }

    /// Creates the slot if needed and puts it on a clean `branch`. With
    /// `avoid_merged`, a branch whose remote tip was already merged as a PR is
    /// not continued; the work starts afresh from the default branch on
    /// `<branch>-2`.
    fn prepare_slot(
        &self,
        repo: &RepoConfig,
        index: usize,
        branch: &str,
        avoid_merged: bool,
    ) -> Result<(Slot, slots::Reset)> {
        let slot = Slot::new(&self.cfg, repo, index);
        let token = self.gh.token();
        if let Some(bytes) = slots::enforce_disk_cap(&slot, repo)? {
            self.log(&format!(
                "{} slot {index}: removed target/ ({} GB)",
                repo.short(),
                bytes / (1 << 30)
            ));
        }
        let locks = slots::clear_stale_locks(&slot)?;
        if !locks.is_empty() {
            let names: Vec<_> = locks.iter().map(|p| p.display().to_string()).collect();
            self.log(&format!(
                "{} slot {index}: removed git locks left behind: {}",
                repo.short(),
                names.join(", ")
            ));
        }
        slots::ensure(&slot, repo, token)?;
        let branch = if avoid_merged {
            let b = slots::unmerged_branch(branch, |b| {
                Ok((
                    self.gh.merged_pr_heads(&repo.name, b)?,
                    slots::remote_tip(&slot, b, token)?,
                ))
            })?;
            if b != branch {
                self.log(&format!(
                    "{}: `{branch}` was already merged, working on `{b}`",
                    repo.short()
                ));
            }
            b
        } else {
            branch.to_string()
        };
        // The reset takes the branch from an idle slot that still has it
        // checked out, but never from a running session.
        if let Some(l) = self.state.locks()?.iter().find(|l| {
            l.repo.eq_ignore_ascii_case(&repo.name) && l.slot != index && l.branch == branch
        }) {
            bail!("`{branch}` is in use by {} in slot {}", l.name, l.slot);
        }
        let reset = slots::reset(&slot, &branch, token)?;
        Ok((slot, reset))
    }

    /// Writes the session's settings and opens its tmux window. Returns the
    /// window id and pid.
    fn launch(
        &self,
        repo: &RepoConfig,
        slot: &Slot,
        index: usize,
        l: &Launch,
    ) -> Result<(String, u32)> {
        let (issue, kind, name, prompt) = (l.issue, l.kind, l.name, l.prompt);
        let notify = session::notify_command(&self.cfg, issue, name);
        let hook = |h| crate::hook::command(&self.exe, &self.opts.config, h, &repo.name, index);
        let (stop, answered) = (hook("stop-hook"), hook("prompt-hook"));
        let issuewright = self.exe.to_string_lossy();
        let hooks = session::Hooks {
            notify: &notify,
            stop: &stop,
            prompt: &answered,
            issuewright: &issuewright,
        };
        let settings = session::settings(&self.cfg, repo, slot, &hooks, kind)?;
        let settings_path =
            self.state
                .session_dir()
                .join(format!("{}-{}.settings.json", repo.short(), index));
        std::fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)?;
        let mut env = session::environment(&self.cfg, repo, slot, l.gh_token);
        // The key that lets this session, and only it, use `issuewright session …`.
        let key = session::new_session_id()?;
        crate::tools::write_record(&crate::tools::Record {
            key: key.clone(),
            repo: repo.name.clone(),
            slot: index,
            dir: slot.dir.clone(),
            branch: l.branch.to_string(),
            kind,
        })?;
        env.insert(crate::tools::KEY_ENV.into(), key);
        let system = (kind == Kind::Implement).then(|| crate::tools::system_prompt(&issuewright));
        let argv = session::interactive_argv(
            &self.claude,
            &env,
            session::model_effort(repo, kind),
            (name, &l.start),
            &settings_path,
            (prompt, system.as_deref()),
            session::permission_mode(&self.cfg, repo),
        );
        tmux::new_window(&self.cfg.tmux_session, name, &slot.dir, &argv)
    }

    fn claim(&self, entry: &Entry, snap: &Snapshot, hist: &mut History) -> Result<SlotLock> {
        let issue = snap
            .issues
            .iter()
            .find(|i| i.r().key() == entry.issue.key())
            .context("issue vanished")?;
        let repo = self
            .cfg
            .repo(&entry.issue.repo)
            .context("repo not configured")?
            .clone();
        // A session that moved its unanswered question here is resumed, so
        // it keeps what it already knows.
        let key = History::key(issue.r());
        let mut watch = self.state.ci_watch()?;
        let asked = watch.asked.remove(&key);
        if asked.is_some() {
            self.state.save_ci_watch(&watch)?;
        }
        let earlier = asked
            .filter(|a| Utc::now() - a.at < chrono::Duration::days(ASKED_RESUME_DAYS))
            .and_then(|a| Some((a.branch, watch.sessions.get(&key)?.clone())));
        let index = match &earlier {
            Some((_, conv)) if self.slot_free(&repo, conv.slot)? => conv.slot,
            _ => self.free_slot(&repo)?,
        };
        let branch = match &earlier {
            Some((b, _)) => b.clone(),
            None => slots::branch_name(
                &self.cfg.branch_template,
                issue.r().number,
                &issue.status.title,
                &issue.status.labels,
            ),
        };
        let (slot, reset) = self.prepare_slot(&repo, index, &branch, true)?;
        let resumed = match &earlier {
            Some((_, conv)) if slot.dir != conv.dir => {
                session::move_transcript(&conv.session_id, &conv.dir, &slot.dir)?
                    .then(|| (conv.session_id.clone(), Some(conv.dir.clone())))
            }
            Some((_, conv)) => session::transcript_dir(&slot.dir)
                .join(format!("{}.jsonl", conv.session_id))
                .is_file()
                .then(|| (conv.session_id.clone(), None)),
            None => None,
        };

        let name = issue.r().short();
        let prompt = match &resumed {
            Some((_, moved_from)) => session::render_answered_prompt(&session::AnsweredVars {
                repo: &issue.r().repo,
                number: issue.r().number,
                dir: &slot.dir,
                branch: &reset.branch,
                moved_from: moved_from.as_deref(),
            }),
            None => session::render_prompt(
                &session::load_template(&self.cfg, Kind::Implement)?,
                &session::PromptVars {
                    repo: &issue.r().repo,
                    number: issue.r().number,
                    title: &issue.status.title,
                    mode: self.opts.mode,
                    branch: &reset.branch,
                    continued: reset.continued,
                    epic: session::epic_text(snap, issue.parent.as_ref()),
                    blockers: session::blockers_text(snap, &issue.blockers()),
                },
            ),
        };

        // Claim on GitHub, then start the session.
        let session_id = match &resumed {
            Some((id, _)) => id.clone(),
            None => session::new_session_id()?,
        };
        let resuming = resumed.is_some();
        let start = match resumed {
            Some(_) => session::Start::Resume(&session_id),
            None => session::Start::New(&session_id),
        };
        self.gh
            .edit_labels(issue.r(), &[RUNNING], &[READY, NEEDS_INPUT])?;
        let started = self.launch(
            &repo,
            &slot,
            index,
            &Launch {
                issue: issue.r(),
                kind: Kind::Implement,
                name: &name,
                prompt: &prompt,
                gh_token: &self.session_token()?,
                branch: &reset.branch,
                start,
            },
        );
        let (window, pid) = match started {
            Ok(x) => x,
            Err(e) => {
                let _ = self.gh.edit_labels(issue.r(), &[NEEDS_INPUT], &[RUNNING]);
                let _ = self.gh.comment(
                    issue.r(),
                    &format!("issuewright couldn't start a session: {e:#}"),
                );
                return Err(e);
            }
        };
        let lock = SlotLock {
            repo: repo.name.clone(),
            slot: index,
            issue: issue.r().clone(),
            mode: self.opts.mode,
            branch: reset.branch.clone(),
            name: name.clone(),
            tmux_window: Some(window.clone()),
            pid: Some(pid),
            started_at: Utc::now(),
            kind: Kind::Implement,
            review: None,
            done_since: None,
            session_id: Some(session_id.clone()),
        };
        self.state.write_lock(&lock)?;
        self.remember_conversation(issue.r(), &session_id, index, &slot)?;
        if let Some(epic) = &issue.parent {
            hist.epic_first_claim
                .entry(History::key(epic))
                .or_insert_with(Utc::now);
            self.state.save_history(hist)?;
        }
        let from = if reset.continued {
            format!("continuing the existing remote branch `{}`", reset.branch)
        } else {
            format!(
                "new branch `{}` from `{}`",
                reset.branch, reset.default_branch
            )
        };
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
        let conversation = if resuming {
            "\n- Conversation: resumed the session that asked the question here, to carry on with the answer"
        } else {
            ""
        };
        self.gh.comment(
            issue.r(),
            &format!(
                "issuewright claimed this issue.\n\n- Mode: {}\n- Slot: {} {index} on `{}`\n- Branch: {from}\n- Session: tmux \
                 `{}:{name}`, Remote Control name `{name}`{conversation}\n- Log: `{}`",
                self.opts.mode,
                repo.short(),
                host.trim(),
                self.cfg.tmux_session,
                self.log_path.display()
            ),
        )?;
        self.log(&format!(
            "{name}: claimed, slot {index}, window {window}, branch {}{}",
            reset.branch,
            if resuming {
                ", resumed the conversation that asked"
            } else {
                ""
            }
        ));
        Ok(lock)
    }

    fn slot_free(&self, repo: &RepoConfig, index: usize) -> Result<bool> {
        Ok(index <= repo.slots
            && !self
                .state
                .locks()?
                .iter()
                .any(|l| l.repo.eq_ignore_ascii_case(&repo.name) && l.slot == index))
    }

    /// Interactive mode: tells each session whose question has waited
    /// `question_timeout_secs` for an answer to move it to GitHub and stop,
    /// which frees its slot. issuewright remembers the conversation; the issue's
    /// next claim resumes it.
    fn check_questions(&self) -> Result<()> {
        let timeout = self.cfg.question_timeout_secs;
        if timeout == 0 || self.opts.mode != Mode::Interactive {
            return Ok(());
        }
        let now = Utc::now();
        for lock in self.state.locks()? {
            let Some(window) = &lock.tmux_window else {
                continue;
            };
            let Some(mut w) = self.state.waiting(&lock.repo, lock.slot) else {
                continue;
            };
            if w.since < lock.started_at {
                self.state.clear_waiting(&lock.repo, lock.slot)?;
                continue;
            }
            if w.nudged_at.is_some() {
                continue;
            }
            // Someone answered in a way the prompt hook doesn't see (a `!`
            // command, a session started without the hook): wait anew.
            if let Some(t) = self.last_input(&lock)
                && t > w.since + chrono::Duration::seconds(5)
            {
                w.since = t;
                self.state.set_waiting(&lock.repo, lock.slot, &w)?;
            }
            if (now - w.since).num_seconds() < timeout as i64 {
                continue;
            }
            if let Err(e) = tmux::send_line(window, &question_handoff(&lock, timeout)) {
                self.log(&format!(
                    "{}: couldn't hand its question over: {e:#}",
                    lock.name
                ));
                continue;
            }
            w.nudged_at = Some(now);
            self.state.set_waiting(&lock.repo, lock.slot, &w)?;
            if lock.kind == Kind::Implement {
                let mut cw = self.state.ci_watch()?;
                cw.asked.insert(
                    History::key(&lock.issue),
                    Asked {
                        at: now,
                        branch: lock.branch.clone(),
                    },
                );
                self.state.save_ci_watch(&cw)?;
            }
            self.log(&format!(
                "{}: no answer for {} min, told it to move its question to GitHub",
                lock.name,
                timeout.div_ceil(60)
            ));
        }
        Ok(())
    }

    /// When the session of `lock` last got input, from its transcript.
    fn last_input(&self, lock: &SlotLock) -> Option<DateTime<Utc>> {
        let repo = self.cfg.repo(&lock.repo)?;
        session::last_input(&Slot::new(&self.cfg, repo, lock.slot).dir)
    }

    /// Records when each session's issue reached its end label, and closes
    /// windows that have been done for `close_done_after_secs`. A session
    /// whose issue is closed (its PR merged, or closed by hand) has nothing
    /// left to do: its window closes at once, whatever the setting.
    fn close_done(&self, snap: &Snapshot) -> Result<()> {
        let windows = tmux::window_ids();
        let now = Utc::now();
        let locks = self.state.locks()?;
        // An issue missing from the snapshot is closed or lost its labels.
        let missing: Vec<IssueRef> = locks
            .iter()
            .filter(|l| snap.labeled(&l.issue).is_none())
            .map(|l| l.issue.clone())
            .collect();
        let briefs = if missing.is_empty() {
            BTreeMap::new()
        } else {
            self.gh.issue_briefs(&missing)?
        };
        for mut lock in locks {
            let done = session_done(&lock, snap);
            if done != lock.done_since.is_some() {
                lock.done_since = done.then_some(now);
                self.state.write_lock(&lock)?;
            }
            let ended = ended_reason(lock.kind, briefs.get(&lock.issue.key()));
            let Some(window) = &lock.tmux_window else {
                continue;
            };
            if Self::alive(&lock, windows.as_ref()) != Some(true) {
                continue;
            }
            let Some(why) =
                close_reason(lock.done_since, ended, self.cfg.close_done_after_secs, now)
            else {
                continue;
            };
            tmux::kill_window(window)?;
            self.log(&format!("{}: {why}, closed its window", lock.name));
        }
        Ok(())
    }

    /// At start: decides which `auto_merge` repositories get reviews, and
    /// warns about those that won't merge automatically, or would merge
    /// without waiting for anything.
    fn check_auto_merge(&mut self) {
        let repos: Vec<String> = self
            .cfg
            .repos
            .iter()
            .filter(|r| r.auto_merge)
            .map(|r| r.name.clone())
            .collect();
        for name in repos {
            let warn =
                |msg: String| self.log(&format!("warning: {name} has auto_merge, but {msg}"));
            let reviews = match &self.app {
                None => {
                    warn("no [review_app] is configured, so reviews are disabled and nothing merges automatically".into());
                    false
                }
                Some(app) => match app.installation(&name) {
                    Ok(Some(_)) => true,
                    Ok(None) => {
                        warn(format!(
                            "the review app ({}) isn't installed on it, so reviews are disabled there",
                            app.slug
                        ));
                        false
                    }
                    Err(e) => {
                        warn(format!(
                            "checking the review app's installation failed, so reviews are disabled there: {e:#}"
                        ));
                        false
                    }
                },
            };
            match self.gh.merge_gates(&name) {
                Ok(g) => {
                    let method = self
                        .cfg
                        .repo(&name)
                        .map(|r| r.merge_method)
                        .unwrap_or_default();
                    for msg in gate_warnings(&g, method) {
                        warn(msg);
                    }
                    if let Some(app) = &self.app
                        && let Some(msg) = app_approval_warning(&g, &app.slug, &app.contents)
                    {
                        warn(msg);
                    }
                }
                Err(e) => warn(format!("its merge settings couldn't be read: {e:#}")),
            }
            if reviews {
                self.review_repos.insert(name.to_lowercase());
            }
        }
    }

    /// Starts an epic-check session for each epic whose sub-issues are all
    /// closed, while slots are free. The epics are those issuewright scheduled
    /// sub-issues of (its history) and those carrying a `dispatch:*` label.
    fn start_epic_checks(&mut self, snap: &Snapshot) -> Result<()> {
        let mut epics: BTreeMap<(String, u64), IssueRef> = BTreeMap::new();
        for key in self.state.history()?.epic_first_claim.keys() {
            if let Some((repo, n)) = key.rsplit_once('#')
                && let Ok(n) = n.parse()
            {
                let r = IssueRef::new(repo, n);
                epics.insert(r.key(), r);
            }
        }
        for i in snap.issues.iter().filter(|i| i.sub_issue_count > 0) {
            epics.insert(i.r().key(), i.r().clone());
        }
        let locks = self.state.locks()?;
        epics.retain(|k, r| {
            !self.closed_epics.contains(k)
                && self.cfg.repo(&r.repo).is_some()
                && !locks.iter().any(|l| l.issue.key() == *k)
        });
        if epics.is_empty() {
            return Ok(());
        }
        let refs: Vec<IssueRef> = epics.into_values().collect();
        let briefs = self.gh.issue_briefs(&refs)?;
        for b in briefs.values() {
            if b.state != IssueState::Open {
                self.closed_epics.insert(b.r.key());
                continue;
            }
            if !epic_needs_check(b) {
                continue;
            }
            let repo = self
                .cfg
                .repo(&b.r.repo)
                .context("repo not configured")?
                .clone();
            if self.backing_off(&b.r) || !self.has_capacity(&repo)? {
                continue;
            }
            let res = self.claim_epic_check(b, &repo);
            self.attempted(&b.r, res, "starting the epic check");
        }
        Ok(())
    }

    fn claim_epic_check(&self, epic: &IssueBrief, repo: &RepoConfig) -> Result<()> {
        let index = self.free_slot(repo)?;
        let (slot, reset) =
            self.prepare_slot(repo, index, &format!("check/{}", epic.r.number), false)?;
        let name = format!("{}-epic", epic.r.short());
        let template = session::load_template(&self.cfg, Kind::EpicCheck)?;
        let prompt = session::render_epic_check_prompt(
            &template,
            &session::EpicCheckVars {
                repo: &epic.r.repo,
                number: epic.r.number,
                title: &epic.title,
                branch: &reset.branch,
                default_branch: &reset.default_branch,
            },
        );
        let session_id = session::new_session_id()?;
        self.gh
            .edit_labels(&epic.r, &[RUNNING], &[READY, NEEDS_INPUT])?;
        let started = self.launch(
            repo,
            &slot,
            index,
            &Launch {
                issue: &epic.r,
                kind: Kind::EpicCheck,
                name: &name,
                prompt: &prompt,
                gh_token: &self.session_token()?,
                branch: &reset.branch,
                start: session::Start::New(&session_id),
            },
        );
        let (window, pid) = match started {
            Ok(x) => x,
            Err(e) => {
                let _ = self.gh.edit_labels(&epic.r, &[NEEDS_INPUT], &[RUNNING]);
                return Err(e);
            }
        };
        self.state.write_lock(&SlotLock {
            repo: repo.name.clone(),
            slot: index,
            issue: epic.r.clone(),
            mode: self.opts.mode,
            branch: reset.branch.clone(),
            name: name.clone(),
            tmux_window: Some(window.clone()),
            pid: Some(pid),
            started_at: Utc::now(),
            kind: Kind::EpicCheck,
            review: None,
            done_since: None,
            session_id: Some(session_id),
        })?;
        self.gh.comment(
            &epic.r,
            &format!(
                "All {} sub-issues are closed. issuewright started a session that checks this epic's acceptance criteria \
                 against `{}` and closes it when they are met (slot {} {index}, tmux `{}:{name}`).",
                epic.sub_issues,
                reset.default_branch,
                repo.short(),
                self.cfg.tmux_session,
            ),
        )?;
        self.log(&format!(
            "{name}: checking the epic, slot {index}, window {window}"
        ));
        Ok(())
    }

    /// Starts a reviewer session for each `dispatch:review` issue of an
    /// `auto_merge` repository whose own session is gone, while slots are free.
    /// Reviews go before new claims: finishing work beats starting it.
    fn start_reviews(&self, snap: &Snapshot) -> Result<()> {
        if self.review_repos.is_empty() {
            return Ok(());
        }
        let mut todo: Vec<&Issue> = snap
            .issues
            .iter()
            .filter(|i| needs_review(&i.status))
            .filter(|i| self.review_repos.contains(&i.r().repo.to_lowercase()))
            .collect();
        todo.sort_by_key(|i| (i.status.priority(), i.status.created_at));
        for issue in todo {
            let locks = self.state.locks()?;
            if locks.iter().any(|l| l.issue.key() == issue.r().key()) {
                continue;
            }
            let repo = self
                .cfg
                .repo(&issue.r().repo)
                .context("repo not configured")?
                .clone();
            if self.backing_off(issue.r())
                || !self.has_capacity(&repo)?
                || !self.still_awaits_review(issue.r())
            {
                continue;
            }
            let pr = match self.gh.open_pr(issue.r()) {
                Ok(Some(pr)) if !pr.draft => pr,
                Ok(Some(pr)) => {
                    // A draft may still become ready; say so once and wait.
                    self.comment_once(
                        issue.r(),
                        &format!("draft:{}", pr.r),
                        &format!(
                            "issuewright can't review this issue for `auto_merge`: {} is a draft. It starts the review \
                             once the PR is ready.",
                            pr.r
                        ),
                    )?;
                    continue;
                }
                Ok(None) => {
                    // Nothing will ever be reviewed or merged for it: partial
                    // PRs (`Part of #N`) were merged, or the PR doesn't say
                    // `Closes #N`. Hand it back instead of waiting forever.
                    self.log(&format!(
                        "{}: dispatch:review without a PR that closes it, needs input",
                        issue.r()
                    ));
                    self.gh
                        .edit_labels(issue.r(), &[NEEDS_INPUT], &[REVIEW, APPROVED])?;
                    self.gh.comment(
                        issue.r(),
                        &format!(
                            "This issue is `{REVIEW}`, but no open pull request in {} closes it (`Closes #{}`) and \
                             targets the default branch, so issuewright has nothing to review or merge. Partial PRs \
                             (`Part of #{}`) leave the issue open. If work is left, set `{READY}`: a new session \
                             continues from the current default branch, or splits the rest into sub-issues. If it \
                             is done, close the issue.",
                            issue.r().repo,
                            issue.r().number,
                            issue.r().number
                        ),
                    )?;
                    continue;
                }
                Err(e) => {
                    self.error(&format!("{}: looking up its PR failed: {e:#}", issue.r()));
                    continue;
                }
            };
            let template = session::load_template(&self.cfg, Kind::Review)?;
            let started = self.claim_review(&issue.status, &repo, &pr, |branch, review| {
                session::render_review_prompt(
                    &template,
                    &session::ReviewVars {
                        repo: &issue.r().repo,
                        number: issue.r().number,
                        title: &issue.status.title,
                        branch,
                        review,
                        epic: session::epic_text(snap, issue.parent.as_ref()),
                    },
                )
            });
            self.attempted(issue.r(), started, "starting the review");
        }
        self.start_pr_reviews(snap)
    }

    /// Starts a reviewer session for each `dispatch:review` PR of an
    /// `auto_merge` repository that closes no issue, such as a partial
    /// `Part of #N` PR. Its labels live on the PR; merging it leaves the
    /// issues it is part of open.
    fn start_pr_reviews(&self, snap: &Snapshot) -> Result<()> {
        let mut todo: Vec<&LabeledPr> = snap
            .prs
            .iter()
            .filter(|p| p.closes.is_empty() && needs_review(&p.status))
            .filter(|p| self.review_repos.contains(&p.status.r.repo.to_lowercase()))
            .collect();
        todo.sort_by_key(|p| (p.status.priority(), p.status.created_at));
        for p in todo {
            let r = &p.status.r;
            if self.state.locks()?.iter().any(|l| l.issue.key() == r.key()) {
                continue;
            }
            let repo = self
                .cfg
                .repo(&r.repo)
                .context("repo not configured")?
                .clone();
            if self.backing_off(r) || !self.has_capacity(&repo)? || !self.still_awaits_review(r) {
                continue;
            }
            if !p.own_into_default {
                self.log(&format!(
                    "{r}: dispatch:review on a PR from another repository or not into the default branch, needs input"
                ));
                self.gh
                    .edit_labels(r, &[NEEDS_INPUT], &[REVIEW, APPROVED])?;
                self.gh.comment(
                    r,
                    &format!(
                        "issuewright reviews and merges only pull requests from a branch of {} into its default branch, \
                         so it won't review this one.",
                        r.repo
                    ),
                )?;
                continue;
            }
            if p.pr.draft {
                self.comment_once(
                    r,
                    &format!("draft:{r}"),
                    "issuewright can't review this PR for `auto_merge` while it is a draft. It starts the review once \
                     the PR is ready.",
                )?;
                continue;
            }
            let template = session::load_pr_review_template(&self.cfg)?;
            let started = self.claim_review(&p.status, &repo, &p.pr, |branch, review| {
                session::render_pr_review_prompt(
                    &template,
                    &session::PrReviewVars {
                        repo: &r.repo,
                        title: &p.status.title,
                        branch,
                        review,
                    },
                )
            });
            self.attempted(r, started, "starting the review");
        }
        Ok(())
    }

    /// Starts a reviewer session on `pr`. `subject` holds the labels: the
    /// issue the PR closes, or the PR itself. `prompt` renders the session's
    /// prompt from the branch and the reviewed commit.
    fn claim_review(
        &self,
        subject: &IssueStatus,
        repo: &RepoConfig,
        pr: &PullRequest,
        prompt: impl FnOnce(&str, &ReviewedPr) -> String,
    ) -> Result<()> {
        // The reviewer acts as the review app, so its approval counts.
        let app_token = self
            .app
            .as_ref()
            .context("reviews need review_app")?
            .token(&repo.name)?;
        let index = self.free_slot(repo)?;
        let (slot, reset) = self.prepare_slot(repo, index, &pr.head_ref, false)?;
        let name = format!("{}-review", subject.r.short());
        let review = ReviewedPr {
            pr: pr.r.clone(),
            head_sha: pr.head_sha.clone(),
        };
        let prompt = prompt(&reset.branch, &review);
        // A verdict from before this review doesn't count.
        if subject.has_label(APPROVED) {
            self.gh.edit_labels(&subject.r, &[], &[APPROVED])?;
        }
        let session_id = session::new_session_id()?;
        let (window, pid) = self.launch(
            repo,
            &slot,
            index,
            &Launch {
                issue: &subject.r,
                kind: Kind::Review,
                name: &name,
                prompt: &prompt,
                gh_token: &app_token,
                branch: &reset.branch,
                start: session::Start::New(&session_id),
            },
        )?;
        self.state.write_lock(&SlotLock {
            repo: repo.name.clone(),
            slot: index,
            issue: subject.r.clone(),
            mode: self.opts.mode,
            branch: reset.branch.clone(),
            name: name.clone(),
            tmux_window: Some(window.clone()),
            pid: Some(pid),
            started_at: Utc::now(),
            kind: Kind::Review,
            review: Some(review.clone()),
            done_since: None,
            session_id: Some(session_id),
        })?;
        self.gh.comment(
            &subject.r,
            &format!(
                "issuewright started a reviewer session for {} at `{}` (slot {} {index}, tmux `{}:{name}`). \
                 If it sets `{APPROVED}`, issuewright enables auto-merge on exactly that commit; otherwise it sets `{NEEDS_INPUT}`.",
                review.pr,
                review.head_sha,
                repo.short(),
                self.cfg.tmux_session,
            ),
        )?;
        self.log(&format!(
            "{name}: reviewing {} at {}, slot {index}, window {window}",
            review.pr, review.head_sha
        ));
        Ok(())
    }

    /// Whether `r` still awaits a review, from its current labels. The poll's
    /// snapshot can predate this poll's own merges: an approved issue whose
    /// reviewer just ended is `dispatch:merging` (or closed) by now, and must
    /// not get a second reviewer, or be handed back for lacking an open PR.
    fn still_awaits_review(&self, r: &IssueRef) -> bool {
        match self.gh.state_and_labels(r) {
            Ok(Some((open, labels))) => awaits_review(open, &labels),
            Ok(None) => false,
            Err(e) => {
                self.error(&format!("{r}: reading its labels failed: {e:#}"));
                false
            }
        }
    }

    /// Whether `r` is still `dispatch:running`, from its current labels.
    /// When GitHub can't be asked, the snapshot's answer (yes) stands.
    fn still_running(&self, r: &IssueRef) -> bool {
        match self.gh.state_and_labels(r) {
            Ok(Some((open, labels))) => open && has_label(&labels, RUNNING),
            Ok(None) => false,
            Err(e) => {
                self.error(&format!("{r}: reading its labels failed: {e:#}"));
                true
            }
        }
    }

    /// Records the implementing session of `issue`, to resume it when CI
    /// fails.
    fn remember_conversation(
        &self,
        issue: &IssueRef,
        session_id: &str,
        index: usize,
        slot: &Slot,
    ) -> Result<()> {
        let mut w = self.state.ci_watch()?;
        w.sessions.insert(
            History::key(issue),
            Conversation {
                session_id: session_id.to_string(),
                slot: index,
                dir: slot.dir.clone(),
            },
        );
        self.state.save_ci_watch(&w)
    }

    /// Watches the checks of the PRs of `dispatch:ci` issues, which no
    /// session holds a slot for. Green: `dispatch:review`, so the review
    /// starts. Red: resumes the conversation that pushed, with the failed
    /// checks. Still running: waits for the next poll.
    fn check_ci(&self, snap: &Snapshot) -> Result<()> {
        let waiting: Vec<&Issue> = snap
            .issues
            .iter()
            .filter(|i| i.status.state == IssueState::Open && i.status.has_label(CI))
            .collect();
        let mut w = self.state.ci_watch()?;
        let keys: BTreeSet<String> = waiting.iter().map(|i| History::key(i.r())).collect();
        w.seen.retain(|k, _| keys.contains(k));
        self.state.save_ci_watch(&w)?;
        for issue in waiting {
            if self
                .state
                .locks()?
                .iter()
                .any(|l| l.issue.key() == issue.r().key())
            {
                continue;
            }
            if self.backing_off(issue.r()) {
                continue;
            }
            let res = self.check_ci_of(issue);
            self.attempted(issue.r(), res, "checking CI");
        }
        Ok(())
    }

    fn check_ci_of(&self, issue: &Issue) -> Result<()> {
        let r = issue.r();
        let Some(pr) = self.gh.open_pr(r)? else {
            return self.hand_back(
                r,
                CI,
                &format!(
                    "This issue is `{CI}`, but no open pull request in {} closes it (`Closes #{}`) and targets the default branch, so issuewright has no checks to wait for. Open the PR and set `{CI}` again, or set `{READY}` for a new session.",
                    r.repo, r.number
                ),
            );
        };
        let checks = self.gh.checks(&pr.r)?;
        let key = History::key(r);
        let mut w = self.state.ci_watch()?;
        let seen = match w.seen.get(&key) {
            Some(s) if s.head_sha == checks.head_sha => s.clone(),
            _ => {
                let s = CiSeen {
                    head_sha: checks.head_sha.clone(),
                    at: Utc::now(),
                };
                w.seen.insert(key.clone(), s.clone());
                self.state.save_ci_watch(&w)?;
                s
            }
        };
        let verdict = ci_verdict(checks.state.as_deref(), Utc::now() - seen.at);
        if verdict == CiVerdict::Pending || !self.still_waits_for_ci(r) {
            return Ok(());
        }
        if verdict == CiVerdict::Green {
            self.gh.edit_labels(r, &[REVIEW], &[CI])?;
            let what = if checks.state.is_some() {
                "CI is green"
            } else {
                "No checks reported"
            };
            self.gh.comment(
                r,
                &format!(
                    "{what} on {} at `{}`, so the PR is ready for review: `{REVIEW}`.",
                    pr.r, checks.head_sha
                ),
            )?;
            self.log(&format!("{r}: {what} on {}, {REVIEW}", pr.r));
            return Ok(());
        }
        let repo = self
            .cfg
            .repo(&r.repo)
            .context("repo not configured")?
            .clone();
        let Some(conv) = w.sessions.get(&key).cloned() else {
            return self.hand_back(
                r,
                CI,
                &format!(
                    "CI failed on {} at `{}`:\n\n{}\n\nissuewright has no session of this issue to resume (it started before issuewright recorded them). Fix it, or set `{READY}` for a new session on the branch.",
                    pr.r,
                    checks.head_sha,
                    failed_list(&checks)
                ),
            );
        };
        if !self.has_capacity(&repo)? {
            return Ok(());
        }
        self.resume(issue, &repo, &pr, &conv, &Wake::CiFailed(&checks))
    }

    /// Resumes the conversation `conv` of `issue` for `wake`: in its old slot
    /// when that is free, else in another one, with the transcript copied
    /// there.
    fn resume(
        &self,
        issue: &Issue,
        repo: &RepoConfig,
        pr: &PullRequest,
        conv: &Conversation,
        wake: &Wake,
    ) -> Result<()> {
        let r = issue.r();
        let what = wake.what(&pr.r);
        let locks = self.state.locks()?;
        let old_free = conv.slot <= repo.slots
            && !locks
                .iter()
                .any(|l| l.repo.eq_ignore_ascii_case(&repo.name) && l.slot == conv.slot);
        let index = if old_free {
            conv.slot
        } else {
            self.free_slot(repo)?
        };
        let (slot, reset) = self.prepare_slot(repo, index, &pr.head_ref, false)?;
        let moved = slot.dir != conv.dir;
        let found = if moved {
            session::move_transcript(&conv.session_id, &conv.dir, &slot.dir)?
        } else {
            session::transcript_dir(&slot.dir)
                .join(format!("{}.jsonl", conv.session_id))
                .is_file()
        };
        if !found {
            return self.hand_back(
                r,
                wake.label(),
                &format!(
                    "{what}\n\nissuewright couldn't resume the session that wrote it: its transcript `{}` is gone. \
                     {}",
                    conv.session_id,
                    wake.by_hand()
                ),
            );
        }
        let moved_from = moved.then_some(conv.dir.as_path());
        let prompt = match wake {
            Wake::CiFailed(checks) => session::render_ci_failed_prompt(&session::CiFailedVars {
                mode: self.opts.mode,
                repo: &r.repo,
                number: r.number,
                pr: &pr.r,
                head_sha: &checks.head_sha,
                failed: &checks.failed,
                dir: &slot.dir,
                branch: &reset.branch,
                moved_from,
            }),
            Wake::Conflicts { head_sha } => session::render_rebase_prompt(&session::RebaseVars {
                mode: self.opts.mode,
                repo: &r.repo,
                number: r.number,
                pr: &pr.r,
                head_sha,
                default_branch: &reset.default_branch,
                dir: &slot.dir,
                branch: &reset.branch,
                moved_from,
            }),
        };
        let name = r.short();
        self.gh.edit_labels(r, &[RUNNING], &[wake.label()])?;
        let started = self.launch(
            repo,
            &slot,
            index,
            &Launch {
                issue: r,
                kind: Kind::Implement,
                name: &name,
                prompt: &prompt,
                gh_token: &self.session_token()?,
                branch: &reset.branch,
                start: session::Start::Resume(&conv.session_id),
            },
        );
        let (window, pid) = match started {
            Ok(x) => x,
            Err(e) => {
                let _ = self.gh.edit_labels(r, &[NEEDS_INPUT], &[RUNNING]);
                let _ = self.gh.comment(
                    r,
                    &format!(
                        "{what}\n\nissuewright couldn't resume the session that wrote it: {e:#}"
                    ),
                );
                return Err(e);
            }
        };
        self.state.write_lock(&SlotLock {
            repo: repo.name.clone(),
            slot: index,
            issue: r.clone(),
            mode: self.opts.mode,
            branch: reset.branch.clone(),
            name: name.clone(),
            tmux_window: Some(window.clone()),
            pid: Some(pid),
            started_at: Utc::now(),
            kind: Kind::Implement,
            review: None,
            done_since: None,
            session_id: Some(conv.session_id.clone()),
        })?;
        self.remember_conversation(r, &conv.session_id, index, &slot)?;
        self.gh.comment(
            r,
            &format!(
                "{what}\n\nissuewright resumed the session that wrote it (slot {} {index}, tmux `{}:{name}`) to {}.",
                repo.short(),
                self.cfg.tmux_session,
                wake.task()
            ),
        )?;
        self.log(&format!(
            "{name}: {}, resumed in slot {index}, window {window}",
            wake.log(&pr.r)
        ));
        Ok(())
    }

    /// Watches approved PRs that wait for auto-merge (`dispatch:merging`).
    /// One that conflicts with the default branch can never merge: issuewright
    /// turns its auto-merge off, so a rebased commit can't merge on the old
    /// approval, and resumes the session that wrote it to rebase.
    fn check_merging(&self, snap: &Snapshot) -> Result<()> {
        for issue in snap
            .issues
            .iter()
            .filter(|i| i.status.state == IssueState::Open && i.status.has_label(MERGING))
        {
            if self
                .state
                .locks()?
                .iter()
                .any(|l| l.issue.key() == issue.r().key())
            {
                continue;
            }
            if self.backing_off(issue.r()) {
                continue;
            }
            let res = self.check_merging_of(issue);
            self.attempted(issue.r(), res, "checking the merge");
        }
        Ok(())
    }

    fn check_merging_of(&self, issue: &Issue) -> Result<()> {
        let r = issue.r();
        // Merged meanwhile: GitHub closes the issue.
        let Some(pr) = self.gh.open_pr(r)? else {
            return Ok(());
        };
        let (mergeable, auto_merge) = self.gh.mergeable(&pr.r)?;
        if mergeable != "CONFLICTING" {
            return Ok(());
        }
        if !matches!(self.gh.state_and_labels(r)?, Some((true, l)) if has_label(&l, MERGING)) {
            return Ok(());
        }
        let repo = self
            .cfg
            .repo(&r.repo)
            .context("repo not configured")?
            .clone();
        let conv = self
            .state
            .ci_watch()?
            .sessions
            .get(&History::key(r))
            .cloned();
        if conv.is_some() && !self.has_capacity(&repo)? {
            return Ok(());
        }
        if auto_merge {
            self.gh.disable_auto_merge(&pr.r)?;
        }
        let wake = Wake::Conflicts {
            head_sha: &pr.head_sha,
        };
        match conv {
            Some(c) => self.resume(issue, &repo, &pr, &c, &wake),
            None => self.hand_back(
                r,
                MERGING,
                &format!(
                    "{}\n\nissuewright has no session of this issue to resume (it started before issuewright recorded \
                     them). {}",
                    wake.what(&pr.r),
                    wake.by_hand()
                ),
            ),
        }
    }

    /// Whether `r` is still open and `dispatch:ci`, from its current labels.
    fn still_waits_for_ci(&self, r: &IssueRef) -> bool {
        matches!(self.gh.state_and_labels(r), Ok(Some((true, labels))) if has_label(&labels, CI))
    }

    /// Swaps `from` for `dispatch:needs-input`, with `body` as the reason.
    fn hand_back(&self, r: &IssueRef, from: &str, body: &str) -> Result<()> {
        self.log(&format!("{r}: {from} can't go on, needs input"));
        self.gh.edit_labels(r, &[NEEDS_INPUT], &[from])?;
        self.gh.comment(r, body)
    }

    /// Merges the PRs that live reviewer sessions approved.
    fn merge_approved(&self, snap: &Snapshot) -> Result<()> {
        for lock in self.state.locks()? {
            if lock.kind != Kind::Review {
                continue;
            }
            if let Some(s) = snap.labeled(&lock.issue)
                && s.has_label(APPROVED)
            {
                self.merge(&lock, &s.r)?;
            }
        }
        Ok(())
    }

    /// Hands the reviewed commit to GitHub's auto-merge with issuewright's own
    /// token. A failure goes back to you as `dispatch:needs-input`.
    /// `subject` holds the labels: the issue, or a PR reviewed on its own.
    fn merge(&self, lock: &SlotLock, subject: &IssueRef) -> Result<()> {
        let Some(rv) = &lock.review else {
            bail!("{}: review session without a PR", lock.name);
        };
        let repo = self.cfg.repo(&lock.repo).context("repo not configured")?;
        // The label alone isn't enough: anyone with the session token can set
        // it. The app's approving review of exactly this commit is.
        let bot = self
            .app
            .as_ref()
            .map(ReviewApp::bot_login)
            .unwrap_or_default();
        if !self.gh.approved_at(&rv.pr, &bot, &rv.head_sha)? {
            self.gh
                .edit_labels(subject, &[NEEDS_INPUT], &[APPROVED, REVIEW])?;
            self.gh.comment(
                subject,
                &format!(
                    "`{APPROVED}` is set, but `{bot}` has no approving review of {} at `{}`, so issuewright didn't merge. \
                     Replace `{NEEDS_INPUT}` with `{REVIEW}` for a fresh review.",
                    rv.pr, rv.head_sha
                ),
            )?;
            self.log(&format!(
                "{}: approved label without {bot}'s approval, not merged",
                lock.name
            ));
            return Ok(());
        }
        // Auto-merge with a method GitHub doesn't accept is enabled, then
        // waits forever. Unreadable settings don't block the merge.
        let merged = match self.gh.merge_gates(&repo.name) {
            Ok(g) if !g.allows(repo.merge_method) => {
                Err(anyhow::anyhow!(method_refused(repo.merge_method, &g)))
            }
            _ => self.gh.merge_pr(&rv.pr, repo.merge_method, &rv.head_sha),
        };
        match merged {
            Ok(how) => {
                let (add, what, log) = merged_report(how);
                self.gh.edit_labels(subject, add, &[APPROVED, REVIEW])?;
                self.gh.comment(
                    subject,
                    &format!(
                        "The reviewer session approved {} at `{}`. {what}",
                        rv.pr, rv.head_sha
                    ),
                )?;
                self.log(&format!("{}: approved, {log} {}", lock.name, rv.pr));
            }
            Err(e) => {
                self.gh
                    .edit_labels(subject, &[NEEDS_INPUT], &[APPROVED, REVIEW])?;
                self.gh.comment(
                    subject,
                    &format!(
                        "The reviewer session approved {} at `{}`, but issuewright couldn't merge it:\n\n```\n{e:#}\n```\n\n\
                         To review it again (for example because the branch moved since the review), replace \
                         `{NEEDS_INPUT}` with `{REVIEW}`.",
                        rv.pr, rv.head_sha
                    ),
                )?;
                self.error(&format!(
                    "{}: approved, but merging failed: {e:#}",
                    lock.name
                ));
            }
        }
        Ok(())
    }

    /// Posts a comment once per `key`.
    fn comment_once(&self, issue: &IssueRef, key: &str, body: &str) -> Result<()> {
        let mut done = self.state.commented()?;
        if done.contains(key) {
            return Ok(());
        }
        self.gh.comment(issue, body)?;
        done.insert(key.to_string());
        self.state.save_commented(&done)
    }
}

/// After a merge of an approved PR: the label to add, the comment's
/// sentence, and the log's words. A PR merged at once needs no
/// `dispatch:merging`; GitHub closes its issue.
fn merged_report(how: Merged) -> (&'static [&'static str], &'static str, &'static str) {
    match how {
        Merged::Auto => (
            &[MERGING],
            "issuewright enabled auto-merge: GitHub merges it once the required checks pass.",
            "auto-merge enabled on",
        ),
        Merged::Now => (
            &[],
            "Its required checks had passed already, so issuewright merged it.",
            "merged",
        ),
    }
}

/// What's wrong with a repository's settings for `auto_merge`.
pub fn gate_warnings(g: &MergeGates, method: MergeMethod) -> Vec<String> {
    let mut out = Vec::new();
    if !g.allows(method) {
        out.push(method_refused(method, g));
    }
    match g.allow_auto_merge {
        Some(false) => out.push(
            "\"Allow auto-merge\" is off in its settings, so every merge fails and goes to dispatch:needs-input".into(),
        ),
        None => out.push("issuewright can't tell whether \"Allow auto-merge\" is on (missing permission?)".into()),
        Some(true) => {}
    }
    if !g.gated() {
        out.push(format!(
            "no branch protection or ruleset on {} requires status checks or reviews, so an approved PR merges at once, without waiting for CI",
            g.default_branch
        ));
    }
    out
}

/// Why a merge with `method` can't work in a repository with gates `g`.
fn method_refused(method: MergeMethod, g: &MergeGates) -> String {
    format!(
        "merge_method \"{}\" isn't allowed into {} (allowed: {}), so auto-merge would be enabled but never merge. \
         Set merge_method in its [[repo]] entry",
        method.name(),
        g.default_branch,
        match g.allowed_methods() {
            m if m.is_empty() => "none".to_string(),
            m => m.join(", "),
        }
    )
}

/// GitHub counts an approval toward required reviews only from a reviewer
/// with write access; for an app that is the Contents permission.
pub fn app_approval_warning(g: &MergeGates, slug: &str, contents: &str) -> Option<String> {
    (g.required_reviews > 0 && contents != "write").then(|| {
        format!(
            "{} requires {} approving review(s) and the review app ({slug}) has Contents: {contents}, so GitHub \
             doesn't count its approval and approved PRs never merge; give the app Contents: Read and write",
            g.default_branch, g.required_reviews
        )
    })
}

/// What a new session runs for.
struct Launch<'a> {
    issue: &'a IssueRef,
    kind: Kind,
    name: &'a str,
    prompt: &'a str,
    /// `GH_TOKEN` of the session.
    gh_token: &'a str,
    /// The branch the slot was reset to.
    branch: &'a str,
    /// A new conversation, or one resumed.
    start: session::Start<'a>,
}

/// Why a live session's window should close now, if it should: its issue
/// is closed, or it has been done for `grace` seconds.
pub fn close_reason(
    done_since: Option<DateTime<Utc>>,
    ended: Option<&str>,
    grace: Option<u64>,
    now: DateTime<Utc>,
) -> Option<String> {
    if let Some(why) = ended {
        return Some(why.into());
    }
    let (since, grace) = (done_since?, grace?);
    (now >= since + chrono::Duration::seconds(grace as i64))
        .then(|| format!("done since {}", since.format("%H:%M")))
}

/// Why a session has nothing left to do, judged from its issue after the
/// issue left the snapshot: it is closed, or it got open sub-issues and lost
/// `dispatch:running` (split into parts, or an epic check created
/// follow-ups), so the next poll schedules those.
pub fn ended_reason(kind: Kind, brief: Option<&IssueBrief>) -> Option<&'static str> {
    let b = brief?;
    if b.state != IssueState::Open {
        return Some("its issue is closed");
    }
    (kind != Kind::Review && b.open_sub_issues() > 0 && !b.has_label(RUNNING))
        .then_some("its issue has open sub-issues to schedule now")
}

/// Whether a session's issue reached the end label of its kind. An issue
/// missing from the snapshot is closed or has lost its `dispatch:*` labels.
pub fn session_done(lock: &SlotLock, snap: &Snapshot) -> bool {
    let Some(s) = snap.labeled(&lock.issue) else {
        return true;
    };
    match lock.kind {
        Kind::Implement => s.has_label(REVIEW) || s.has_label(CI),
        Kind::Review => [APPROVED, MERGING, NEEDS_INPUT]
            .iter()
            .any(|l| s.has_label(l)),
        Kind::EpicCheck => s.has_label(NEEDS_INPUT) || !s.has_label(RUNNING),
    }
}

/// An open epic whose sub-issues are all closed, with no session on it and
/// no question waiting.
pub fn epic_needs_check(b: &IssueBrief) -> bool {
    b.state == IssueState::Open
        && b.sub_issues > 0
        && b.open_sub_issues() == 0
        && !b.has_label(RUNNING)
        && !b.has_label(NEEDS_INPUT)
}

/// Why issuewright resumes an implementing session.
/// How long after moving its question to GitHub a session is still resumed
/// rather than started afresh.
const ASKED_RESUME_DAYS: i64 = 7;

/// What issuewright types into a session whose question went unanswered. One line:
/// tmux submits it with Enter.
fn question_handoff(l: &SlotLock, timeout: u64) -> String {
    let mins = timeout.div_ceil(60);
    let s = &l.issue;
    let ask = format!(
        "issuewright: nobody answered within {mins} minutes, so move your question to GitHub and free this slot for other work."
    );
    match l.kind {
        Kind::Implement => format!(
            "{ask} If you have changes worth keeping, commit them and push them to `{}` with a plain `git push`. Then \
             write the question as a comment on {s}, with everything needed to answer it without this conversation, \
             and set `{NEEDS_INPUT}` (removing `{RUNNING}`). Then stop. When the issue is released again with \
             `{READY}`, issuewright resumes this conversation.",
            l.branch
        ),
        Kind::Review => format!(
            "{ask} Write the question as a comment on {s}, with everything needed to answer it, swap `{REVIEW}` for \
             `{NEEDS_INPUT}`, and stop. Don't approve."
        ),
        Kind::EpicCheck => format!(
            "{ask} Write the question as a comment on {s}, with everything needed to answer it, set `{NEEDS_INPUT}` \
             (removing `{RUNNING}`), and stop."
        ),
    }
}

enum Wake<'a> {
    /// CI failed on the PR's head; the issue was `dispatch:ci`.
    CiFailed(&'a Checks),
    /// The approved PR at `head_sha` conflicts with the default branch; the
    /// issue was `dispatch:merging`, and its auto-merge is off now.
    Conflicts { head_sha: &'a str },
}

impl Wake<'_> {
    /// The label the issue carries while issuewright waits.
    fn label(&self) -> &'static str {
        match self {
            Wake::CiFailed(_) => CI,
            Wake::Conflicts { .. } => MERGING,
        }
    }

    /// What happened to `pr`, for the issue comment.
    fn what(&self, pr: &IssueRef) -> String {
        match self {
            Wake::CiFailed(c) => format!(
                "CI failed on {pr} at `{}`:\n\n{}",
                c.head_sha,
                failed_list(c)
            ),
            Wake::Conflicts { head_sha } => format!(
                "{pr} was approved at `{head_sha}`, but it conflicts with the default branch, so it can't merge. \
                 issuewright turned its auto-merge off, so a rebased commit can't merge on the old approval."
            ),
        }
    }

    /// What the resumed session does.
    fn task(&self) -> &'static str {
        match self {
            Wake::CiFailed(_) => "fix it",
            Wake::Conflicts { .. } => "rebase it",
        }
    }

    /// What a person does when no session can be resumed.
    fn by_hand(&self) -> String {
        match self {
            Wake::CiFailed(_) => {
                format!("Fix it, or set `{READY}` for a new session on the branch.")
            }
            Wake::Conflicts { .. } => format!(
                "Rebase it onto the default branch, then set `{CI}`: issuewright waits for CI and starts a fresh review."
            ),
        }
    }

    fn log(&self, pr: &IssueRef) -> String {
        match self {
            Wake::CiFailed(c) => format!("CI failed on {pr} at {:.7}", c.head_sha),
            Wake::Conflicts { head_sha } => {
                format!("{pr} at {head_sha:.7} conflicts with the default branch")
            }
        }
    }
}

/// How long a PR may report no checks at all before issuewright takes it as a
/// repository without CI, rather than checks that haven't registered yet.
const NO_CHECKS_GRACE_SECS: i64 = 180;

#[derive(Debug, PartialEq, Eq)]
pub enum CiVerdict {
    Pending,
    Green,
    Red,
}

/// The verdict on a PR's check rollup `state`, `waited` after issuewright first
/// saw its head commit.
pub fn ci_verdict(state: Option<&str>, waited: chrono::Duration) -> CiVerdict {
    match state {
        Some("SUCCESS") => CiVerdict::Green,
        Some("FAILURE" | "ERROR") => CiVerdict::Red,
        Some(_) => CiVerdict::Pending,
        None if waited >= chrono::Duration::seconds(NO_CHECKS_GRACE_SECS) => CiVerdict::Green,
        None => CiVerdict::Pending,
    }
}

fn failed_list(c: &Checks) -> String {
    if c.failed.is_empty() {
        return "- (GitHub names no failed check; the rollup reports a failure)".into();
    }
    c.failed
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An open `dispatch:review` issue with no verdict and nothing else going on.
pub fn needs_review(s: &IssueStatus) -> bool {
    awaits_review(s.state == IssueState::Open, &s.labels)
}

/// [`needs_review`] from an issue's or PR's open state and labels.
pub fn awaits_review(open: bool, labels: &[String]) -> bool {
    open && has_label(labels, REVIEW)
        && ![READY, RUNNING, NEEDS_INPUT, MERGING]
            .iter()
            .any(|l| has_label(labels, l))
}

fn has_label(labels: &[String], l: &str) -> bool {
    labels.iter().any(|x| x.eq_ignore_ascii_case(l))
}

pub fn print_plan(plan: &Plan, locks: &[SlotLock]) {
    if !locks.is_empty() {
        println!("Running:");
        for l in locks {
            println!(
                "  {:<28} {:<9} slot {} of {}  since {}",
                l.name,
                l.kind,
                l.slot,
                l.repo,
                l.started_at.format("%Y-%m-%d %H:%M")
            );
        }
        println!();
    }
    if plan.entries.is_empty() {
        println!("No {READY} issues.");
    } else {
        println!("Queue ({READY}, scheduled order):");
        for e in &plan.entries {
            let (tag, why) = match &e.verdict {
                Verdict::Claim => ("CLAIM", String::new()),
                Verdict::Waiting(w) => ("wait ", w.clone()),
                Verdict::Skipped(s) => ("skip ", s.clone()),
            };
            let epic = e
                .epic
                .as_ref()
                .map(|r| format!(" [epic {}]", r.short()))
                .unwrap_or_default();
            println!(
                "  {tag} {:<24} {}{epic}",
                e.issue.short(),
                truncate(&e.title, 60)
            );
            if !why.is_empty() {
                println!("        {why}");
            }
        }
    }
    for n in &plan.notes {
        println!("note: {n}");
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(labels: &[&str]) -> Issue {
        Issue {
            status: IssueStatus {
                r: IssueRef::new("acme/app", 7),
                title: "t".into(),
                state: IssueState::Open,
                labels: labels.iter().map(|l| l.to_string()).collect(),
                created_at: Utc::now(),
                closed_at: None,
                merged_pr: None,
            },
            parent: None,
            sub_issue_count: 0,
            sub_issues_completed: 0,
            native_blockers: vec![],
            body_blockers: vec![],
        }
    }

    #[test]
    fn question_handoff_is_one_line_naming_the_way_back() {
        for kind in [Kind::Implement, Kind::Review, Kind::EpicCheck] {
            let m = question_handoff(&lock(kind), 900);
            assert!(!m.contains('\n'), "{m}");
            assert!(m.contains("15 minutes") && m.contains(NEEDS_INPUT), "{m}");
        }
        let m = question_handoff(&lock(Kind::Implement), 900);
        assert!(m.contains("git push") && m.contains("feat/7-t") && m.contains(READY));
    }

    fn lock(kind: Kind) -> SlotLock {
        // Written before `kind`, `review` and `done_since` existed.
        let mut l: SlotLock = serde_json::from_str(
            r#"{"repo":"acme/app","slot":1,"issue":{"repo":"acme/app","number":7},"mode":"interactive",
                "branch":"feat/7-t","name":"app#7","tmux_window":"@1","pid":1,"started_at":"2026-11-16T08:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(l.kind, Kind::Implement);
        l.kind = kind;
        l
    }

    fn snap(labels: &[&str]) -> Snapshot {
        Snapshot {
            issues: vec![issue(labels)],
            ..Default::default()
        }
    }

    #[test]
    fn implement_session_is_done_at_review_or_when_closed() {
        let l = lock(Kind::Implement);
        assert!(!session_done(&l, &snap(&[RUNNING])));
        assert!(!session_done(&l, &snap(&[NEEDS_INPUT])));
        assert!(session_done(&l, &snap(&[REVIEW])));
        assert!(session_done(&l, &Snapshot::default()));
    }

    #[test]
    fn review_session_is_done_at_its_verdict() {
        let l = lock(Kind::Review);
        assert!(!session_done(&l, &snap(&[REVIEW])));
        assert!(session_done(&l, &snap(&[REVIEW, APPROVED])));
        assert!(session_done(&l, &snap(&[MERGING])));
        assert!(session_done(&l, &snap(&[NEEDS_INPUT])));
        assert!(session_done(&l, &Snapshot::default()));
    }

    /// A PR reviewed on its own carries the labels itself; the lock is keyed
    /// on the PR, not on an issue.
    #[test]
    fn pr_review_session_is_done_at_the_prs_verdict() {
        let l = lock(Kind::Review);
        let pr_snap = |labels: &[&str]| {
            let status = issue(labels).status;
            Snapshot {
                prs: vec![LabeledPr {
                    pr: PullRequest {
                        r: status.r.clone(),
                        head_ref: "feat/x".into(),
                        head_sha: "sha".into(),
                        draft: false,
                    },
                    status,
                    closes: vec![],
                    own_into_default: true,
                }],
                ..Default::default()
            }
        };
        assert!(!session_done(&l, &pr_snap(&[REVIEW])));
        assert!(session_done(&l, &pr_snap(&[REVIEW, APPROVED])));
        assert!(needs_review(&pr_snap(&[REVIEW]).prs[0].status));
        assert!(!needs_review(
            &pr_snap(&[REVIEW, NEEDS_INPUT]).prs[0].status
        ));
    }

    #[test]
    fn closed_issue_closes_at_once_done_one_after_the_grace_period() {
        let now = Utc::now();
        let min = |m: i64| Some(now - chrono::Duration::minutes(m));
        // Closed: at once, even without close_done_after_secs or a done time.
        assert!(
            close_reason(None, Some("its issue is closed"), None, now)
                .is_some_and(|w| w.contains("closed"))
        );
        assert!(close_reason(min(0), Some("x"), Some(600), now).is_some());
        // Done: only after the grace period, and only when it is set.
        assert!(close_reason(min(5), None, Some(600), now).is_none());
        assert!(close_reason(min(11), None, Some(600), now).is_some());
        assert!(close_reason(min(60), None, None, now).is_none());
        assert!(close_reason(None, None, Some(0), now).is_none());
    }

    /// Live: `ISSUEWRIGHT_LIVE_REPOS=owner/a,owner/b cargo test live_gate -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_gate_warnings() {
        let gh = Gh::new(std::env::var("ISSUEWRIGHT_LIVE_GH_USER").ok().as_deref()).unwrap();
        for repo in std::env::var("ISSUEWRIGHT_LIVE_REPOS").unwrap().split(',') {
            let g = gh.merge_gates(repo).unwrap();
            println!("{repo}: {g:?}");
            for w in gate_warnings(&g, MergeMethod::Squash) {
                println!("  warning: {w}");
            }
        }
    }

    #[test]
    fn warns_when_the_apps_approval_wont_count() {
        let g = |reviews: u64| MergeGates {
            default_branch: "main".into(),
            allow_auto_merge: Some(true),
            required_checks: true,
            required_reviews: reviews,
            ..Default::default()
        };
        assert!(
            app_approval_warning(&g(1), "rev", "read")
                .is_some_and(|w| w.contains("Contents: read"))
        );
        assert!(app_approval_warning(&g(1), "rev", "write").is_none());
        assert!(app_approval_warning(&g(0), "rev", "read").is_none());
    }

    #[test]
    fn warns_about_missing_auto_merge_and_ungated_branches() {
        let g = |allow: Option<bool>, checks: bool, reviews: u64| MergeGates {
            default_branch: "main".into(),
            allow_auto_merge: allow,
            required_checks: checks,
            required_reviews: reviews,
            ..Default::default()
        };
        let sq = MergeMethod::Squash;
        assert!(gate_warnings(&g(Some(true), true, 0), sq).is_empty());
        assert!(gate_warnings(&g(Some(true), false, 1), sq).is_empty());
        let mut only_merge = g(Some(true), true, 0);
        only_merge.ruleset_methods = Some(vec!["merge".into()]);
        let w = gate_warnings(&only_merge, sq);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("\"squash\" isn't allowed into main (allowed: merge)"),
            "{}",
            w[0]
        );
        assert!(gate_warnings(&only_merge, MergeMethod::Merge).is_empty());
        let w = gate_warnings(&g(Some(false), false, 0), sq);
        assert_eq!(w.len(), 2);
        assert!(w[0].contains("Allow auto-merge") && w[1].contains("without waiting for CI"));
        assert!(gate_warnings(&g(None, true, 0), sq)[0].contains("can't tell"));
    }

    fn brief(state: IssueState, labels: &[&str], subs: u64, closed: u64) -> IssueBrief {
        IssueBrief {
            r: IssueRef::new("acme/app", 7),
            title: "t".into(),
            state,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            sub_issues: subs,
            sub_issues_closed: closed,
        }
    }

    #[test]
    fn closed_or_split_issue_ends_its_session() {
        use IssueState::*;
        let ended = |k, b: IssueBrief| ended_reason(k, Some(&b));
        for k in [Kind::Implement, Kind::Review, Kind::EpicCheck] {
            assert!(ended(k, brief(ClosedCompleted, &[], 0, 0)).is_some());
            assert!(ended(k, brief(ClosedOther, &[], 0, 0)).is_some());
        }
        // Split: open sub-issues and no longer running.
        assert!(ended(Kind::Implement, brief(Open, &[], 3, 0)).is_some());
        assert!(ended(Kind::EpicCheck, brief(Open, &[], 4, 3)).is_some());
        // Still creating parts, or nothing open to schedule.
        assert!(ended(Kind::Implement, brief(Open, &[RUNNING], 3, 0)).is_none());
        assert!(ended(Kind::EpicCheck, brief(Open, &[], 3, 3)).is_none());
        // A reviewer's issue never gets split.
        assert!(ended(Kind::Review, brief(Open, &[], 3, 0)).is_none());
        assert!(ended_reason(Kind::Implement, None).is_none());
    }

    #[test]
    fn epic_is_checked_once_all_sub_issues_are_closed() {
        use IssueState::*;
        assert!(epic_needs_check(&brief(Open, &[], 3, 3)));
        assert!(epic_needs_check(&brief(
            Open,
            &[READY, "priority:P1"],
            1,
            1
        )));
        assert!(!epic_needs_check(&brief(Open, &[], 3, 2)));
        assert!(!epic_needs_check(&brief(Open, &[], 0, 0)));
        assert!(!epic_needs_check(&brief(ClosedCompleted, &[], 3, 3)));
        assert!(!epic_needs_check(&brief(Open, &[RUNNING], 3, 3)));
        assert!(!epic_needs_check(&brief(Open, &[NEEDS_INPUT], 3, 3)));
    }

    #[test]
    fn epic_check_is_done_once_it_stops_running() {
        let l = lock(Kind::EpicCheck);
        assert!(!session_done(&l, &snap(&[RUNNING])));
        assert!(session_done(&l, &snap(&[NEEDS_INPUT])));
        assert!(session_done(&l, &snap(&[READY])));
        assert!(session_done(&l, &Snapshot::default()));
    }

    #[test]
    fn review_needed_only_without_a_verdict_or_other_work() {
        assert!(needs_review(&issue(&[REVIEW]).status));
        // An approval nobody asked for doesn't skip the review.
        assert!(needs_review(&issue(&[REVIEW, APPROVED]).status));
        for other in [READY, RUNNING, NEEDS_INPUT, MERGING] {
            assert!(!needs_review(&issue(&[REVIEW, other]).status), "{other}");
        }
        assert!(!needs_review(&issue(&[]).status));
        let mut closed = issue(&[REVIEW]).status;
        closed.state = IssueState::ClosedCompleted;
        assert!(!needs_review(&closed));
    }

    #[test]
    fn ci_verdicts() {
        let s = chrono::Duration::seconds;
        assert_eq!(ci_verdict(Some("SUCCESS"), s(0)), CiVerdict::Green);
        assert_eq!(ci_verdict(Some("FAILURE"), s(0)), CiVerdict::Red);
        assert_eq!(ci_verdict(Some("ERROR"), s(0)), CiVerdict::Red);
        assert_eq!(ci_verdict(Some("PENDING"), s(9999)), CiVerdict::Pending);
        assert_eq!(ci_verdict(Some("EXPECTED"), s(9999)), CiVerdict::Pending);
        // No checks yet: maybe not registered; after the grace, none exist.
        assert_eq!(ci_verdict(None, s(10)), CiVerdict::Pending);
        assert_eq!(ci_verdict(None, s(NO_CHECKS_GRACE_SECS)), CiVerdict::Green);
    }

    #[test]
    fn implement_session_is_done_at_ci() {
        let l = lock(Kind::Implement);
        assert!(session_done(&l, &snap(&[CI])));
        assert!(!session_done(&l, &snap(&[RUNNING])));
    }

    #[test]
    fn merged_after_approval_awaits_no_second_review() {
        let l = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // What the poll's stale snapshot shows, and what GitHub shows after
        // the same poll enabled auto-merge, or GitHub merged at once.
        assert!(awaits_review(true, &l(&[REVIEW, APPROVED])));
        assert!(!awaits_review(true, &l(&[MERGING])));
        assert!(!awaits_review(false, &l(&[MERGING])));
    }
}
