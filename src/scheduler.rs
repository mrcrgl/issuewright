//! The scheduler: a pure function from a snapshot of issues to an ordered plan.
//!
//! 1. Candidates: open `dispatch:ready` issues, minus `status:blocked` and epics.
//! 2. Dependencies: runnable only when every blocker is done.
//! 3. Epic precedence: sub-issues of started epics first, epics by priority,
//!    then by start time; optional limit on started epics.
//! 4. Order inside a group: the epic's sub-issue order, then priority, then age.
//! 5. Capacity: fill free slots from the top, per repo and globally, while
//!    fewer of the repo's issues than `max_in_ci` wait for CI, unless new
//!    sessions are on hold (resources) or the issue waits after failed starts.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BlockerDone;
use crate::model::*;

#[derive(Clone, Debug)]
pub struct Policy {
    pub blocker_done: BlockerDone,
    pub max_started_epics: Option<usize>,
    pub max_sessions: usize,
    /// `max_in_ci` per repository, keyed by the lowercase `owner/name`.
    pub max_in_ci: BTreeMap<String, usize>,
    /// Why no new session may start at all, such as a nearly full disk.
    pub hold: Option<String>,
    /// Issues that wait before their next attempt, with why.
    pub held: BTreeMap<(String, u64), String>,
    /// Consider only issues of this priority or more urgent (P0 is 0).
    pub only_priority: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Capacity {
    /// Sessions running across all repositories.
    pub running: usize,
    /// Free slots per repository, keyed by the lowercase `owner/name`.
    pub free_slots: BTreeMap<String, usize>,
}

/// What issuewright remembers itself, to complement what GitHub can tell.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// When issuewright first claimed a sub-issue of an epic, keyed by the epic.
    pub epic_first_claim: BTreeMap<String, DateTime<Utc>>,
}

impl History {
    pub fn key(r: &IssueRef) -> String {
        format!("{}#{}", r.repo.to_lowercase(), r.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    Claim,
    Waiting(String),
    Skipped(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub issue: IssueRef,
    pub title: String,
    pub epic: Option<IssueRef>,
    pub verdict: Verdict,
}

/// A blocker that can't be resolved, so the dependent waits. issuewright comments
/// once on the dependent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unresolvable {
    pub dependent: IssueRef,
    pub blocker: IssueRef,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    /// Scheduled order: claims and waiting issues interleaved as ranked,
    /// skipped issues last.
    pub entries: Vec<Entry>,
    pub notes: Vec<String>,
    pub unresolvable: Vec<Unresolvable>,
}

impl Plan {
    pub fn claims(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.verdict == Verdict::Claim)
    }
}

struct EpicInfo {
    r: IssueRef,
    started: bool,
    started_at: DateTime<Utc>,
    priority: u8,
    has_open_subs: bool,
}

struct Cand<'a> {
    issue: &'a Issue,
    epic: Option<(String, u64)>,
    epic_index: usize,
    blocked: Option<String>,
}

fn counts_as_started(s: &IssueStatus) -> bool {
    s.state != IssueState::Open
        || s.merged_pr.is_some()
        || [
            RUNNING,
            CI,
            REVIEW,
            NEEDS_INPUT,
            APPROVED,
            MERGING,
            STATUS_IN_PROGRESS,
            STATUS_REVIEW,
        ]
        .iter()
        .any(|l| s.has_label(l))
}

fn epic_infos(snap: &Snapshot, hist: &History) -> BTreeMap<(String, u64), EpicInfo> {
    snap.epics
        .iter()
        .map(|(k, e)| {
            let local = hist
                .epic_first_claim
                .get(&History::key(&e.status.r))
                .copied();
            let started = local.is_some() || e.sub_issues.iter().any(counts_as_started);
            // Started at the earliest close among its sub-issues or issuewright's
            // first claim; GitHub doesn't say when a human started one, so the
            // epic's creation time stands in.
            let started_at = e
                .sub_issues
                .iter()
                .filter_map(|s| s.closed_at)
                .chain(local)
                .min()
                .unwrap_or(e.status.created_at);
            let info = EpicInfo {
                r: e.status.r.clone(),
                started,
                started_at,
                priority: e.status.priority(),
                has_open_subs: e.sub_issues.iter().any(|s| s.state == IssueState::Open),
            };
            (k.clone(), info)
        })
        .collect()
}

/// Whether `s`, as a blocker, no longer holds its dependents back.
pub fn blocker_done(s: &IssueStatus, policy: &Policy) -> bool {
    s.state == IssueState::ClosedCompleted
        || (policy.blocker_done == BlockerDone::MergedOrClosed && s.merged_pr.is_some())
}

fn blocker_reason(
    issue: &Issue,
    snap: &Snapshot,
    policy: &Policy,
    unresolvable: &mut Vec<Unresolvable>,
) -> Option<String> {
    let mut reasons = Vec::new();
    for b in issue.blockers() {
        if let Some(why) = snap.unresolved.get(&b.key()) {
            unresolvable.push(Unresolvable {
                dependent: issue.r().clone(),
                blocker: b.clone(),
                reason: why.clone(),
            });
            reasons.push(format!("blocker {} can't be resolved ({why})", b.short()));
            continue;
        }
        let Some(s) = snap.status(&b) else {
            unresolvable.push(Unresolvable {
                dependent: issue.r().clone(),
                blocker: b.clone(),
                reason: "not found".into(),
            });
            reasons.push(format!(
                "blocker {} can't be resolved (not found)",
                b.short()
            ));
            continue;
        };
        let merged_counts = policy.blocker_done == BlockerDone::MergedOrClosed;
        if !blocker_done(s, policy) {
            let what = match s.state {
                IssueState::Open if merged_counts => "open, no merged PR",
                IssueState::Open => "open",
                _ => "closed as not planned",
            };
            reasons.push(format!("waiting on {} ({what})", b.short()));
        }
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

pub fn schedule(snap: &Snapshot, policy: &Policy, cap: &Capacity, hist: &History) -> Plan {
    let mut plan = Plan::default();
    let mut epics = epic_infos(snap, hist);
    let mut skipped = Vec::new();
    let mut cands: Vec<Cand> = Vec::new();

    // 1. Candidates, 2. dependencies.
    for issue in &snap.issues {
        let s = &issue.status;
        if !s.has_label(READY) || s.state != IssueState::Open {
            continue;
        }
        let epic = issue
            .parent
            .as_ref()
            .map(|p| p.key())
            .filter(|k| epics.contains_key(k));
        let skip = if s.has_label(STATUS_BLOCKED) {
            Some("labeled status:blocked".to_string())
        } else if issue.sub_issue_count > 0 {
            Some("epic; only its sub-issues are scheduled".to_string())
        } else if s.has_label(TYPE_EPIC) {
            Some(format!(
                "labeled {TYPE_EPIC} but has no sub-issues on GitHub; link them as sub-issues to schedule them"
            ))
        } else if [RUNNING, NEEDS_INPUT, CI, REVIEW, APPROVED, MERGING]
            .iter()
            .any(|l| s.has_label(l))
        {
            Some("carries another dispatch:* label too".to_string())
        } else {
            None
        };
        if let Some(reason) = skip {
            skipped.push(Entry {
                issue: s.r.clone(),
                title: s.title.clone(),
                epic: issue.parent.clone(),
                verdict: Verdict::Skipped(reason),
            });
            continue;
        }
        let epic_index = epic
            .as_ref()
            .and_then(|k| {
                snap.epics[k]
                    .sub_issues
                    .iter()
                    .position(|x| x.r.key() == s.r.key())
            })
            .unwrap_or(usize::MAX);
        let blocked = blocker_reason(issue, snap, policy, &mut plan.unresolvable);
        cands.push(Cand {
            issue,
            epic,
            epic_index,
            blocked,
        });
    }

    // 3. Epic precedence and 4. order inside a group.
    let by_age = |a: &Cand, b: &Cand| -> Ordering {
        let (x, y) = (&a.issue.status, &b.issue.status);
        x.priority()
            .cmp(&y.priority())
            .then(x.created_at.cmp(&y.created_at))
            .then(x.r.cmp(&y.r))
    };
    let (mut started, mut rest): (Vec<Cand>, Vec<Cand>) = cands
        .into_iter()
        .partition(|c| c.epic.as_ref().is_some_and(|k| epics[k].started));
    started.sort_by(|a, b| {
        let (ea, eb) = (
            &epics[a.epic.as_ref().unwrap()],
            &epics[b.epic.as_ref().unwrap()],
        );
        ea.priority
            .cmp(&eb.priority)
            .then(ea.started_at.cmp(&eb.started_at))
            .then(ea.r.cmp(&eb.r))
            .then(a.epic_index.cmp(&b.epic_index))
            .then_with(|| by_age(a, b))
    });
    rest.sort_by(by_age);
    // Sub-issues of the same unstarted epic keep their places in the ranking
    // but take them in the epic's own order.
    let mut slots_by_epic: BTreeMap<(String, u64), Vec<usize>> = BTreeMap::new();
    for (i, c) in rest.iter().enumerate() {
        if let Some(k) = &c.epic {
            slots_by_epic.entry(k.clone()).or_default().push(i);
        }
    }
    let mut rest: Vec<Option<Cand>> = rest.into_iter().map(Some).collect();
    for positions in slots_by_epic.values() {
        let mut group: Vec<Cand> = positions.iter().map(|&i| rest[i].take().unwrap()).collect();
        group.sort_by(|a, b| a.epic_index.cmp(&b.epic_index).then_with(|| by_age(a, b)));
        for (&i, c) in positions.iter().zip(group) {
            rest[i] = Some(c);
        }
    }
    let ordered: Vec<Cand> = started
        .into_iter()
        .chain(rest.into_iter().flatten())
        .collect();

    // 5. Capacity, with the limit on started epics.
    let mut in_ci: BTreeMap<String, usize> = BTreeMap::new();
    for i in &snap.issues {
        if i.status.state == IssueState::Open && i.status.has_label(CI) {
            *in_ci.entry(i.status.r.repo.to_lowercase()).or_default() += 1;
        }
    }
    let mut running = cap.running;
    let mut free = cap.free_slots.clone();
    let mut started_open = epics
        .values()
        .filter(|e| e.started && e.has_open_subs)
        .count();
    for c in &ordered {
        let s = &c.issue.status;
        let repo = s.r.repo.to_lowercase();
        let epic_started = c.epic.as_ref().map(|k| epics[k].started);
        let verdict = if let Some(why) = &c.blocked {
            Verdict::Waiting(why.clone())
        } else if let Some(max) = policy.only_priority
            && s.priority() > max
        {
            Verdict::Waiting(format!("only P{max} issues are claimed first"))
        } else if let Some(why) = &policy.hold {
            Verdict::Waiting(why.clone())
        } else if let Some(why) = policy.held.get(&s.r.key()) {
            Verdict::Waiting(why.clone())
        } else if let (Some(limit), Some(false)) = (policy.max_started_epics, epic_started)
            && started_open >= limit
        {
            let e = &epics[c.epic.as_ref().unwrap()].r;
            Verdict::Waiting(format!(
                "epic {} not started, max_started_epics ({limit}) reached",
                e.short()
            ))
        } else if let Some(&max) = policy.max_in_ci.get(&repo)
            && in_ci.get(&repo).copied().unwrap_or(0) >= max
        {
            Verdict::Waiting(format!(
                "{} PRs wait for CI, max_in_ci ({max}) reached",
                in_ci[&repo]
            ))
        } else if running >= policy.max_sessions {
            Verdict::Waiting(format!("max_sessions ({}) reached", policy.max_sessions))
        } else if !free.contains_key(&repo) {
            Verdict::Waiting(format!("{} is not configured", s.r.repo))
        } else if free[&repo] == 0 {
            let name = s.r.repo.rsplit('/').next().unwrap_or(&s.r.repo);
            Verdict::Waiting(format!("no free slot in {name}"))
        } else {
            running += 1;
            *free.get_mut(&repo).unwrap() -= 1;
            if let Some(k) = &c.epic
                && !epics[k].started
            {
                let e = epics.get_mut(k).unwrap();
                e.started = true;
                if e.has_open_subs {
                    started_open += 1;
                }
            }
            Verdict::Claim
        };
        plan.entries.push(Entry {
            issue: s.r.clone(),
            title: s.title.clone(),
            epic: c.epic.as_ref().map(|k| epics[k].r.clone()),
            verdict,
        });
    }

    // Notes for started epics that can't make progress right now.
    let mut noted = BTreeSet::new();
    for e in epics.values().filter(|e| e.started && e.has_open_subs) {
        let mine: Vec<&Entry> = plan
            .entries
            .iter()
            .filter(|x| x.epic.as_ref().map(|r| r.key()) == Some(e.r.key()))
            .collect();
        if mine.iter().any(|x| x.verdict == Verdict::Claim) || !noted.insert(e.r.key()) {
            continue;
        }
        if mine.is_empty() {
            if policy.max_started_epics.is_some() {
                plan.notes.push(format!("epic {} is started, but none of its open sub-issues is {READY}", e.r.short()));
            }
        } else if mine.iter().all(|x| {
            matches!(&x.verdict, Verdict::Waiting(w) if w.starts_with("waiting on") || w.starts_with("blocker"))
        }) {
            plan.notes.push(format!("epic {} is started; all its ready sub-issues wait on blockers", e.r.short()));
        }
    }
    plan.entries.extend(skipped);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const MONO: &str = "acme/mono";
    const WEB: &str = "acme/web";

    fn t(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
    }

    fn st(repo: &str, n: u64, labels: &[&str], day: u32) -> IssueStatus {
        IssueStatus {
            r: IssueRef::new(repo, n),
            title: format!("issue {n}"),
            state: IssueState::Open,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            created_at: t(day),
            closed_at: None,
            merged_pr: None,
        }
    }

    fn issue(repo: &str, n: u64, labels: &[&str], day: u32) -> Issue {
        Issue {
            status: st(repo, n, labels, day),
            parent: None,
            sub_issue_count: 0,
            sub_issues_completed: 0,
            native_blockers: vec![],
            body_blockers: vec![],
        }
    }

    fn policy() -> Policy {
        Policy {
            blocker_done: BlockerDone::MergedOrClosed,
            max_started_epics: None,
            max_sessions: 10,
            max_in_ci: BTreeMap::new(),
            hold: None,
            held: BTreeMap::new(),
            only_priority: None,
        }
    }

    fn cap(mono: usize, web: usize) -> Capacity {
        Capacity {
            running: 0,
            free_slots: [(MONO.to_string(), mono), (WEB.to_string(), web)].into(),
        }
    }

    fn add_epic(snap: &mut Snapshot, repo: &str, n: u64, labels: &[&str], subs: Vec<IssueStatus>) {
        let e = Epic {
            status: st(repo, n, labels, 1),
            sub_issues: subs,
        };
        snap.epics.insert(IssueRef::new(repo, n).key(), e);
    }

    fn verdicts(p: &Plan) -> Vec<(u64, Verdict)> {
        p.entries
            .iter()
            .map(|e| (e.issue.number, e.verdict.clone()))
            .collect()
    }

    fn claimed(p: &Plan) -> Vec<u64> {
        p.claims().map(|e| e.issue.number).collect()
    }

    fn run(snap: &Snapshot, cap: &Capacity) -> Plan {
        schedule(snap, &policy(), cap, &History::default())
    }

    #[test]
    fn skips_blocked_epics_and_unlabeled() {
        let mut snap = Snapshot::default();
        snap.issues
            .push(issue(MONO, 1, &[READY, STATUS_BLOCKED], 1));
        let mut epic = issue(MONO, 2, &[READY], 1);
        epic.sub_issue_count = 3;
        snap.issues.push(epic);
        snap.issues.push(issue(MONO, 3, &[RUNNING], 1));
        snap.issues.push(issue(MONO, 4, &[READY], 1));
        // An epic by label only, even at top priority, is never claimed.
        snap.issues
            .push(issue(MONO, 5, &[READY, TYPE_EPIC, "priority:P0"], 1));
        let p = run(&snap, &cap(5, 5));
        assert_eq!(claimed(&p), [4]);
        assert!(p.entries.iter().any(|e| e.issue.number == 5
            && matches!(&e.verdict, Verdict::Skipped(r) if r.contains(TYPE_EPIC))));
        assert!(matches!(&verdicts(&p)[1].1, Verdict::Skipped(r) if r.contains("status:blocked")));
        assert!(matches!(&verdicts(&p)[2].1, Verdict::Skipped(r) if r.contains("epic")));
        assert_eq!(p.entries.len(), 4);
    }

    #[test]
    fn a_sub_issue_never_waits_on_its_parent() {
        let mut snap = Snapshot::default();
        let mut sub = issue(MONO, 12, &[READY], 1);
        sub.parent = Some(IssueRef::new(MONO, 10));
        // "None beyond #10's landed slices": the parent in the body section.
        sub.body_blockers.push(IssueRef::new(MONO, 10));
        sub.native_blockers.push(IssueRef::new(MONO, 10));
        assert!(sub.blockers().is_empty());
        snap.others
            .insert(IssueRef::new(MONO, 10).key(), st(MONO, 10, &[], 1));
        snap.issues.push(sub);
        let p = run(&snap, &cap(5, 5));
        assert_eq!(claimed(&p), [12]);
    }

    #[test]
    fn native_dependency_waits_until_merged_or_closed() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[READY], 1));
        let mut b = issue(MONO, 2, &[READY], 1);
        b.native_blockers.push(IssueRef::new(MONO, 1));
        snap.issues.push(b);
        let p = run(&snap, &cap(5, 5));
        assert_eq!(claimed(&p), [1]);
        assert_eq!(
            verdicts(&p)[1].1,
            Verdict::Waiting("waiting on mono#1 (open, no merged PR)".into())
        );

        // A's PR merged: B runs with merged_or_closed, not with closed.
        snap.issues[0].status.labels = vec![REVIEW.into()];
        snap.issues[0].status.merged_pr = Some(IssueRef::new(MONO, 9));
        assert_eq!(claimed(&run(&snap, &cap(5, 5))), [2]);
        let closed_only = Policy {
            blocker_done: BlockerDone::Closed,
            ..policy()
        };
        let p = schedule(&snap, &closed_only, &cap(5, 5), &History::default());
        assert_eq!(
            verdicts(&p)[0].1,
            Verdict::Waiting("waiting on mono#1 (open)".into())
        );

        // A closed: B runs with closed.
        snap.issues[0].status.state = IssueState::ClosedCompleted;
        let p = schedule(&snap, &closed_only, &cap(5, 5), &History::default());
        assert_eq!(claimed(&p), [2]);
    }

    #[test]
    fn closed_as_not_planned_keeps_waiting() {
        let mut snap = Snapshot::default();
        let mut a = st(MONO, 1, &[], 1);
        a.state = IssueState::ClosedOther;
        snap.others.insert(a.r.key(), a);
        let mut b = issue(MONO, 2, &[READY], 1);
        b.native_blockers.push(IssueRef::new(MONO, 1));
        snap.issues.push(b);
        let p = run(&snap, &cap(5, 5));
        assert_eq!(
            verdicts(&p)[0].1,
            Verdict::Waiting("waiting on mono#1 (closed as not planned)".into())
        );
    }

    #[test]
    fn body_dependency_counts_like_native() {
        // The parsing of the body section itself is tested in body.rs; here
        // the scheduler treats body blockers like native ones.
        let mut snap = Snapshot::default();
        snap.others
            .insert(IssueRef::new(WEB, 5).key(), st(WEB, 5, &[], 1));
        let mut b = issue(MONO, 2, &[READY], 1);
        b.body_blockers.push(IssueRef::new(WEB, 5));
        snap.issues.push(b);
        let p = run(&snap, &cap(5, 5));
        assert_eq!(
            verdicts(&p)[0].1,
            Verdict::Waiting("waiting on web#5 (open, no merged PR)".into())
        );
    }

    #[test]
    fn started_epic_goes_before_higher_priority_work() {
        let mut snap = Snapshot::default();
        let mut done = st(MONO, 11, &[], 1);
        done.state = IssueState::ClosedCompleted;
        done.closed_at = Some(t(2));
        add_epic(
            &mut snap,
            MONO,
            10,
            &["priority:P3"],
            vec![done, st(MONO, 12, &[READY], 3)],
        );
        add_epic(
            &mut snap,
            MONO,
            20,
            &["priority:P0"],
            vec![st(MONO, 21, &[READY], 1)],
        );
        let mut s12 = issue(MONO, 12, &[READY, "priority:P4"], 3);
        s12.parent = Some(IssueRef::new(MONO, 10));
        let mut s21 = issue(MONO, 21, &[READY, "priority:P0"], 1);
        s21.parent = Some(IssueRef::new(MONO, 20));
        snap.issues
            .extend([s21, issue(MONO, 30, &[READY, "priority:P0"], 1), s12]);
        let p = run(&snap, &cap(1, 0));
        assert_eq!(claimed(&p), [12]);
        let order: Vec<u64> = p.entries.iter().map(|e| e.issue.number).collect();
        assert_eq!(order[0], 12);
    }

    #[test]
    fn older_started_epic_first_at_equal_priority() {
        let mut snap = Snapshot::default();
        for (epic, sub, closed_day) in [(10, 11, 5), (20, 21, 3)] {
            let mut done = st(MONO, sub, &[], 1);
            done.state = IssueState::ClosedCompleted;
            done.closed_at = Some(t(closed_day));
            add_epic(
                &mut snap,
                MONO,
                epic,
                &[],
                vec![done, st(MONO, sub + 1, &[READY], 1)],
            );
            let mut s = issue(MONO, sub + 1, &[READY], 1);
            s.parent = Some(IssueRef::new(MONO, epic));
            snap.issues.push(s);
        }
        let p = run(&snap, &cap(1, 0));
        assert_eq!(claimed(&p), [22]);
    }

    #[test]
    fn epic_started_by_running_sub_issue() {
        let mut snap = Snapshot::default();
        add_epic(
            &mut snap,
            MONO,
            10,
            &[],
            vec![st(MONO, 11, &[RUNNING], 1), st(MONO, 12, &[READY], 9)],
        );
        let mut s = issue(MONO, 12, &[READY], 9);
        s.parent = Some(IssueRef::new(MONO, 10));
        snap.issues
            .extend([issue(MONO, 5, &[READY, "priority:P0"], 1), s]);
        assert_eq!(claimed(&run(&snap, &cap(1, 0))), [12]);
    }

    #[test]
    fn sub_issue_order_beats_priority_and_age() {
        let mut snap = Snapshot::default();
        // Epic order: 13, 12, 11. Priorities and ages say otherwise.
        add_epic(
            &mut snap,
            MONO,
            10,
            &[],
            vec![
                st(MONO, 13, &[READY], 9),
                st(MONO, 12, &[READY], 5),
                st(MONO, 11, &[READY], 1),
            ],
        );
        for (n, prio, day) in [
            (11, "priority:P0", 1),
            (12, "priority:P1", 5),
            (13, "priority:P4", 9),
        ] {
            let mut s = issue(MONO, n, &[READY, prio], day);
            s.parent = Some(IssueRef::new(MONO, 10));
            snap.issues.push(s);
        }
        // Unstarted epic: the group still runs in epic order.
        let order: Vec<u64> = run(&snap, &cap(0, 0))
            .entries
            .iter()
            .map(|e| e.issue.number)
            .collect();
        assert_eq!(order, [13, 12, 11]);
        // Started (via local history): same.
        let mut hist = History::default();
        hist.epic_first_claim
            .insert(History::key(&IssueRef::new(MONO, 10)), t(1));
        let p = schedule(&snap, &policy(), &cap(2, 0), &hist);
        assert_eq!(claimed(&p), [13, 12]);
    }

    #[test]
    fn wip_limit_holds_back_unstarted_epics() {
        let mut snap = Snapshot::default();
        add_epic(
            &mut snap,
            MONO,
            10,
            &[],
            vec![st(MONO, 11, &[RUNNING], 1), st(MONO, 12, &[READY], 1)],
        );
        add_epic(&mut snap, MONO, 20, &[], vec![st(MONO, 21, &[READY], 1)]);
        let mut s12 = issue(MONO, 12, &[READY], 1);
        s12.parent = Some(IssueRef::new(MONO, 10));
        s12.native_blockers.push(IssueRef::new(MONO, 11));
        let mut s21 = issue(MONO, 21, &[READY], 1);
        s21.parent = Some(IssueRef::new(MONO, 20));
        snap.issues.extend([s12, s21, issue(MONO, 30, &[READY], 2)]);
        let limited = Policy {
            max_started_epics: Some(1),
            ..policy()
        };
        let p = schedule(&snap, &limited, &cap(5, 5), &History::default());
        // Issues outside any epic are not limited.
        assert_eq!(claimed(&p), [30]);
        let v = verdicts(&p);
        assert_eq!(
            v[1],
            (
                21,
                Verdict::Waiting("epic mono#20 not started, max_started_epics (1) reached".into())
            )
        );
        assert_eq!(
            p.notes,
            ["epic mono#10 is started; all its ready sub-issues wait on blockers"]
        );
        // Without the limit, epic 20 starts.
        assert_eq!(claimed(&run(&snap, &cap(5, 5))), [21, 30]);
    }

    #[test]
    fn wip_limit_counts_epics_started_in_this_cycle() {
        let mut snap = Snapshot::default();
        for epic in [10, 20] {
            add_epic(
                &mut snap,
                MONO,
                epic,
                &[],
                vec![st(MONO, epic + 1, &[READY], 1)],
            );
            let mut s = issue(MONO, epic + 1, &[READY], 1);
            s.parent = Some(IssueRef::new(MONO, epic));
            snap.issues.push(s);
        }
        let limited = Policy {
            max_started_epics: Some(1),
            ..policy()
        };
        let p = schedule(&snap, &limited, &cap(5, 5), &History::default());
        assert_eq!(claimed(&p), [11]);
    }

    #[test]
    fn cross_repo_sub_issue_waits_for_mono_merge() {
        let mut snap = Snapshot::default();
        add_epic(
            &mut snap,
            MONO,
            10,
            &[],
            vec![st(MONO, 11, &[RUNNING], 1), st(WEB, 5, &[READY], 1)],
        );
        let mut w = issue(WEB, 5, &[READY], 1);
        w.parent = Some(IssueRef::new(MONO, 10));
        w.native_blockers.push(IssueRef::new(MONO, 11));
        snap.issues.push(w);
        snap.issues.push(issue(MONO, 11, &[RUNNING], 1));
        let p = run(&snap, &cap(5, 5));
        assert_eq!(
            verdicts(&p)[0].1,
            Verdict::Waiting("waiting on mono#11 (open, no merged PR)".into())
        );
        snap.issues[1].status.merged_pr = Some(IssueRef::new(MONO, 99));
        assert_eq!(claimed(&run(&snap, &cap(5, 5))), [5]);
    }

    #[test]
    fn unresolvable_blocker_waits_and_is_reported() {
        let mut snap = Snapshot::default();
        let ghost = IssueRef::new("other/private", 7);
        snap.unresolved
            .insert(ghost.key(), "repository not readable".into());
        let mut b = issue(MONO, 2, &[READY], 1);
        b.body_blockers.push(ghost.clone());
        snap.issues.push(b);
        let p = run(&snap, &cap(5, 5));
        assert!(
            matches!(&verdicts(&p)[0].1, Verdict::Waiting(w) if w.contains("can't be resolved"))
        );
        assert_eq!(p.unresolvable.len(), 1);
        assert_eq!(p.unresolvable[0].blocker, ghost);
    }

    #[test]
    fn capacity_global_and_per_repo() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[READY, "priority:P0"], 1));
        snap.issues.push(issue(MONO, 2, &[READY, "priority:P1"], 1));
        snap.issues.push(issue(WEB, 3, &[READY, "priority:P2"], 1));
        // One mono slot: the full repo doesn't block web further down.
        let p = run(&snap, &cap(1, 1));
        assert_eq!(claimed(&p), [1, 3]);
        assert_eq!(
            verdicts(&p)[1].1,
            Verdict::Waiting("no free slot in mono".into())
        );
        // Global limit of one.
        let one = Policy {
            max_sessions: 1,
            ..policy()
        };
        let p = schedule(&snap, &one, &cap(5, 5), &History::default());
        assert_eq!(claimed(&p), [1]);
        // One session already running.
        let busy = Capacity {
            running: 1,
            ..cap(5, 5)
        };
        assert!(claimed(&schedule(&snap, &one, &busy, &History::default())).is_empty());
    }

    #[test]
    fn max_in_ci_holds_new_claims_in_that_repo_only() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[CI], 1));
        snap.issues.push(issue(MONO, 2, &[CI], 1));
        snap.issues.push(issue(MONO, 3, &[READY], 1));
        snap.issues.push(issue(WEB, 4, &[READY], 1));
        let limit = |n| Policy {
            max_in_ci: [(MONO.to_string(), n)].into(),
            ..policy()
        };
        let p = schedule(&snap, &limit(2), &cap(5, 5), &History::default());
        assert_eq!(claimed(&p), [4]);
        assert!(verdicts(&p).contains(&(
            3,
            Verdict::Waiting("2 PRs wait for CI, max_in_ci (2) reached".into())
        )));
        let p = schedule(&snap, &limit(3), &cap(5, 5), &History::default());
        assert_eq!(claimed(&p), [3, 4]);
    }

    #[test]
    fn held_issues_and_a_hold_leave_slots_to_others() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[READY], 1));
        snap.issues.push(issue(MONO, 2, &[READY], 1));
        let backoff = Policy {
            held: [(
                (MONO.to_lowercase(), 1),
                "claim failed 2 times, next try at 10:04".to_string(),
            )]
            .into(),
            ..policy()
        };
        let p = schedule(&snap, &backoff, &cap(1, 1), &History::default());
        assert_eq!(claimed(&p), [2]);
        assert!(verdicts(&p).contains(&(
            1,
            Verdict::Waiting("claim failed 2 times, next try at 10:04".into())
        )));
        let hold = Policy {
            hold: Some("/ has 3 GB free".into()),
            ..policy()
        };
        let p = schedule(&snap, &hold, &cap(5, 5), &History::default());
        assert!(claimed(&p).is_empty());
        assert!(verdicts(&p).contains(&(2, Verdict::Waiting("/ has 3 GB free".into()))));
    }

    #[test]
    fn only_priority_leaves_the_rest_waiting_without_using_slots() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[READY, "priority:P2"], 1));
        snap.issues.push(issue(MONO, 2, &[READY, "priority:P0"], 2));
        let urgent = Policy {
            only_priority: Some(0),
            ..policy()
        };
        let p = schedule(&snap, &urgent, &cap(1, 1), &History::default());
        assert_eq!(claimed(&p), [2]);
    }

    #[test]
    fn priority_then_age_outside_epics() {
        let mut snap = Snapshot::default();
        snap.issues.push(issue(MONO, 1, &[READY], 1));
        snap.issues.push(issue(MONO, 2, &[READY, "P3"], 5));
        snap.issues.push(issue(MONO, 3, &[READY, "priority:P3"], 2));
        let order: Vec<u64> = run(&snap, &cap(0, 0))
            .entries
            .iter()
            .map(|e| e.issue.number)
            .collect();
        assert_eq!(order, [3, 2, 1]);
    }
}
