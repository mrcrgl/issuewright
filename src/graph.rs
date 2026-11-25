//! The dependency graph behind the queue, and where each waiting issue's
//! chain of blockers ends.
//!
//! issuewright writes the graph into `status.json` at every poll. `issuewright watch`
//! and `issuewright status` read it to show which issue holds which others back,
//! and which of those chains won't move until you act.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::*;
use crate::scheduler::{Plan, Policy, Verdict, blocker_done};

type Key = (String, u64);
/// The waiting issues right below `of` that `seen` doesn't have yet.
type Children<'a> = dyn Fn(&BTreeSet<Key>, &BTreeSet<Key>) -> Vec<Key> + 'a;

/// One issue in the graph: a `dispatch:*` issue, or a blocker of one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub issue: IssueRef,
    pub title: String,
    pub state: IssueState,
    /// Its `dispatch:*` labels and `status:blocked`.
    pub labels: Vec<String>,
    /// Done as a blocker (`blocker_done`).
    pub done: bool,
    #[serde(default)]
    pub epic: Option<IssueRef>,
    /// Known for `dispatch:*` issues only; empty for the others.
    #[serde(default)]
    pub blockers: Vec<IssueRef>,
    /// Why issuewright couldn't look this blocker up.
    #[serde(default)]
    pub unresolved: Option<String>,
}

impl Node {
    pub fn has(&self, l: &str) -> bool {
        self.labels.iter().any(|x| x.eq_ignore_ascii_case(l))
    }

    /// Ready, otherwise unhindered, and waiting on a blocker: the scheduler
    /// passes it over until its blockers are done.
    fn waits(&self, open_blockers: bool) -> bool {
        open_blockers
            && self.has(READY)
            && !self.has(STATUS_BLOCKED)
            && !ALL_LABELS.iter().any(|l| *l != READY && self.has(l))
    }
}

/// The graph of `snap`'s `dispatch:*` issues and their blockers.
pub fn build(snap: &Snapshot, policy: &Policy) -> Vec<Node> {
    let keep = |labels: &[String]| {
        labels
            .iter()
            .filter(|l| l.starts_with("dispatch:") || l.eq_ignore_ascii_case(STATUS_BLOCKED))
            .cloned()
            .collect()
    };
    let node = |s: &IssueStatus| Node {
        issue: s.r.clone(),
        title: s.title.clone(),
        state: s.state,
        labels: keep(&s.labels),
        done: blocker_done(s, policy),
        epic: None,
        blockers: vec![],
        unresolved: None,
    };
    let mut out: BTreeMap<Key, Node> = BTreeMap::new();
    for i in &snap.issues {
        let mut n = node(&i.status);
        n.epic = i.parent.clone();
        n.blockers = i.blockers();
        out.insert(i.r().key(), n);
    }
    for p in &snap.prs {
        out.entry(p.status.r.key())
            .or_insert_with(|| node(&p.status));
    }
    let wanted: Vec<IssueRef> = out.values().flat_map(|n| n.blockers.clone()).collect();
    for b in wanted {
        if out.contains_key(&b.key()) {
            continue;
        }
        let unresolved = |why: &str| Node {
            issue: b.clone(),
            title: String::new(),
            state: IssueState::Open,
            labels: vec![],
            done: false,
            epic: None,
            blockers: vec![],
            unresolved: Some(why.to_string()),
        };
        let n = match (snap.unresolved.get(&b.key()), snap.status(&b)) {
            (Some(why), _) => unresolved(why),
            (None, Some(s)) => node(s),
            (None, None) => unresolved("not found"),
        };
        out.insert(b.key(), n);
    }
    out.into_values().collect()
}

/// Where a chain of blockers ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// A session or GitHub works on it.
    Moving(String),
    /// Ready with nothing in its way; with the reason when it still waits
    /// for a slot or the epic limit.
    Queued(Option<String>),
    NeedsInput,
    /// `status:blocked`.
    Parked,
    /// Ready, but the scheduler skips it.
    Skipped(String),
    /// Open without a `dispatch:*` label: nothing starts it.
    Unowned,
    /// Closed as not planned or duplicate: it will never be done.
    Dead,
    Unresolvable(String),
    /// Ready issues that wait on each other.
    Cycle(Vec<IssueRef>),
}

impl End {
    /// Moves on without you.
    pub fn flows(&self) -> bool {
        matches!(self, End::Moving(_) | End::Queued(_))
    }

    pub fn describe(&self) -> String {
        match self {
            End::Moving(l) => l.clone(),
            End::Queued(None) => "ready, starts at the next free slot".into(),
            End::Queued(Some(w)) => format!("ready, waits: {w}"),
            End::NeedsInput => format!("{NEEDS_INPUT}: waits for your answer"),
            End::Parked => format!("{STATUS_BLOCKED}: parked by hand"),
            End::Skipped(s) => format!("ready but skipped: {s}"),
            End::Unowned => "open without a dispatch:* label: nothing will start it".into(),
            End::Dead => "closed as not planned: never counts as done".into(),
            End::Unresolvable(w) => format!("can't be looked up ({w})"),
            End::Cycle(m) => format!(
                "deadlock: {} wait on each other",
                m.iter().map(IssueRef::short).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// One waiting issue under the end it waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Tree guides, like `│  └─ `.
    pub guide: String,
    pub issue: IssueRef,
    pub title: String,
    /// Its other open blockers, besides the one it's drawn under.
    pub with: Vec<IssueRef>,
    /// The other ends it waits on too.
    pub also: Vec<IssueRef>,
}

/// A chain end that holds ready issues back.
#[derive(Clone, Debug)]
pub struct Stall {
    /// The end; a cycle's lowest member.
    pub at: IssueRef,
    pub title: String,
    pub end: End,
    /// Issues waiting on it, directly or through others.
    pub tree: Vec<Line>,
    /// How many distinct issues wait on it.
    pub held: usize,
}

/// Every chain end that holds a ready issue back, those that need you first,
/// then by how many issues each holds.
pub fn analyze(nodes: &[Node], plan: &Plan) -> Vec<Stall> {
    let by_key: BTreeMap<Key, &Node> = nodes.iter().map(|n| (n.issue.key(), n)).collect();
    let verdict: BTreeMap<Key, &Verdict> = plan
        .entries
        .iter()
        .map(|e| (e.issue.key(), &e.verdict))
        .collect();

    // Edges from a waiting issue to its open blockers.
    let edges: BTreeMap<Key, Vec<Key>> = nodes
        .iter()
        .map(|n| {
            let open: Vec<Key> = n
                .blockers
                .iter()
                .map(IssueRef::key)
                .filter(|k| by_key.get(k).is_some_and(|b| !b.done))
                .collect();
            let out = if n.waits(!open.is_empty()) {
                open
            } else {
                vec![]
            };
            (n.issue.key(), out)
        })
        .collect();
    let reach = |from: &Key| -> BTreeSet<Key> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<&Key> = edges[from].iter().collect();
        while let Some(k) = todo.pop() {
            if seen.insert(k.clone()) {
                todo.extend(edges[k].iter());
            }
        }
        seen
    };
    let reaches: BTreeMap<Key, BTreeSet<Key>> =
        edges.keys().map(|k| (k.clone(), reach(k))).collect();

    // The end a node stands for: its cycle's lowest member, or itself.
    let cycle_of = |k: &Key| -> Option<Vec<Key>> {
        reaches[k].contains(k).then(|| {
            reaches[k]
                .iter()
                .filter(|c| reaches[*c].contains(k))
                .cloned()
                .collect()
        })
    };
    let end_id = |k: &Key| -> Option<Key> {
        match cycle_of(k) {
            Some(members) => members.into_iter().next(),
            None if edges[k].is_empty() => Some(k.clone()),
            None => None,
        }
    };

    // The ends each waiting issue waits on.
    let mut ends_of: BTreeMap<Key, BTreeSet<Key>> = BTreeMap::new();
    for (k, e) in &edges {
        if e.is_empty() {
            continue;
        }
        let ends = reaches[k].iter().filter_map(end_id).collect();
        ends_of.insert(k.clone(), ends);
    }
    let all_ends: BTreeSet<Key> = ends_of.values().flatten().cloned().collect();

    let mut out: Vec<Stall> = all_ends
        .into_iter()
        .map(|id| {
            let members: BTreeSet<Key> = cycle_of(&id)
                .map(|m| m.into_iter().collect())
                .unwrap_or_else(|| [id.clone()].into());
            let node = by_key[&id];
            let end = if members.len() > 1 || edges[&id].contains(&id) {
                End::Cycle(members.iter().map(|k| by_key[k].issue.clone()).collect())
            } else {
                classify(node, verdict.get(&id).copied())
            };
            let dependents: BTreeSet<Key> = ends_of
                .iter()
                .filter(|(k, ends)| ends.contains(&id) && !members.contains(*k))
                .map(|(k, _)| k.clone())
                .collect();
            let mut tree = Vec::new();
            let mut seen = members.clone();
            let line = |k: &Key, under: &BTreeSet<Key>| {
                let n = by_key[k];
                let with = edges[k]
                    .iter()
                    .filter(|b| !under.contains(*b))
                    .map(|b| by_key[b].issue.clone())
                    .collect();
                let also = ends_of[k]
                    .iter()
                    .filter(|e| **e != id)
                    .map(|e| by_key[e].issue.clone())
                    .collect();
                Line {
                    guide: String::new(),
                    issue: n.issue.clone(),
                    title: n.title.clone(),
                    with,
                    also,
                }
            };
            let children = |of: &BTreeSet<Key>, seen: &BTreeSet<Key>| -> Vec<Key> {
                dependents
                    .iter()
                    .filter(|w| !seen.contains(*w) && edges[*w].iter().any(|b| of.contains(b)))
                    .cloned()
                    .collect()
            };
            // Depth-first, each issue once, under the first blocker that
            // leads to it.
            fn walk(
                at: Vec<Key>,
                under: &BTreeSet<Key>,
                prefix: &str,
                seen: &mut BTreeSet<Key>,
                tree: &mut Vec<Line>,
                children: &Children,
                line: &dyn Fn(&Key, &BTreeSet<Key>) -> Line,
            ) {
                let at: Vec<Key> = at.into_iter().filter(|k| seen.insert(k.clone())).collect();
                for (i, k) in at.iter().enumerate() {
                    let last = i + 1 == at.len();
                    tree.push(Line {
                        guide: format!("{prefix}{}", if last { "└─ " } else { "├─ " }),
                        ..line(k, under)
                    });
                    let here: BTreeSet<Key> = [k.clone()].into();
                    let next = children(&here, seen);
                    let deeper = format!("{prefix}{}", if last { "   " } else { "│  " });
                    walk(next, &here, &deeper, seen, tree, children, line);
                }
            }
            let first = children(&members, &seen);
            walk(first, &members, "", &mut seen, &mut tree, &children, &line);
            Stall {
                at: node.issue.clone(),
                title: node.title.clone(),
                end,
                held: dependents.len(),
                tree,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.end.flows(), std::cmp::Reverse(a.held), a.at.key()).cmp(&(
            b.end.flows(),
            std::cmp::Reverse(b.held),
            b.at.key(),
        ))
    });
    out
}

/// Whether the issue waits on an open blocker, as the scheduler sees it.
pub fn waits(nodes: &[Node], r: &IssueRef) -> bool {
    let open = |b: &IssueRef| nodes.iter().any(|n| n.issue.key() == b.key() && !n.done);
    nodes
        .iter()
        .find(|n| n.issue.key() == r.key())
        .is_some_and(|n| n.waits(n.blockers.iter().any(open)))
}

fn classify(n: &Node, verdict: Option<&Verdict>) -> End {
    if let Some(why) = &n.unresolved {
        return End::Unresolvable(why.clone());
    }
    if n.state == IssueState::ClosedOther {
        return End::Dead;
    }
    if n.has(NEEDS_INPUT) {
        return End::NeedsInput;
    }
    if n.has(STATUS_BLOCKED) {
        return End::Parked;
    }
    if let Some(l) = [RUNNING, CI, REVIEW, CHANGES, APPROVED, MERGING]
        .iter()
        .find(|l| n.has(l))
    {
        return End::Moving(l.to_string());
    }
    if n.has(READY) {
        return match verdict {
            Some(Verdict::Waiting(w)) => End::Queued(Some(w.clone())),
            Some(Verdict::Skipped(s)) => End::Skipped(s.clone()),
            _ => End::Queued(None),
        };
    }
    End::Unowned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(number: u64, labels: &[&str], blockers: &[u64]) -> Node {
        Node {
            issue: IssueRef::new("acme/app", number),
            title: format!("t{number}"),
            state: IssueState::Open,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            done: false,
            epic: None,
            blockers: blockers
                .iter()
                .map(|b| IssueRef::new("acme/app", *b))
                .collect(),
            unresolved: None,
        }
    }

    fn nums(s: &Stall) -> Vec<u64> {
        s.tree.iter().map(|l| l.issue.number).collect()
    }

    #[test]
    fn chain_ends_at_the_issue_a_session_works_on() {
        let g = [
            n(1, &[RUNNING], &[]),
            n(2, &[READY], &[1]),
            n(3, &[READY], &[2]),
            n(4, &[READY], &[1]),
        ];
        let s = analyze(&g, &Plan::default());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].at.number, 1);
        assert_eq!(s[0].end, End::Moving(RUNNING.into()));
        assert_eq!(s[0].held, 3);
        assert_eq!(nums(&s[0]), [2, 3, 4]);
        assert_eq!(s[0].tree[1].guide, "│  └─ ");
        assert!(s[0].tree.iter().all(|l| l.with.is_empty()));
        assert_eq!(s[0].tree[2].guide, "└─ ");
    }

    #[test]
    fn unowned_and_dead_blockers_come_first() {
        let mut dead = n(9, &[], &[]);
        dead.state = IssueState::ClosedOther;
        let g = [
            n(1, &[RUNNING], &[]),
            n(2, &[READY], &[1]),
            n(3, &[READY], &[1]),
            n(5, &[], &[]),
            n(6, &[READY], &[5]),
            dead,
            n(10, &[READY], &[9]),
        ];
        let s = analyze(&g, &Plan::default());
        let ends: Vec<_> = s.iter().map(|s| (s.at.number, s.end.flows())).collect();
        assert_eq!(ends, [(5, false), (9, false), (1, true)]);
        assert_eq!(s[0].end, End::Unowned);
        assert_eq!(s[1].end, End::Dead);
    }

    #[test]
    fn ready_issues_waiting_on_each_other_are_a_deadlock() {
        let g = [
            n(1, &[READY], &[2]),
            n(2, &[READY], &[1]),
            n(3, &[READY], &[2]),
        ];
        let s = analyze(&g, &Plan::default());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].at.number, 1);
        assert!(matches!(&s[0].end, End::Cycle(m) if m.len() == 2));
        assert!(!s[0].end.flows());
        assert_eq!(nums(&s[0]), [3]);
        assert_eq!(s[0].held, 1);
    }

    #[test]
    fn an_issue_drawn_under_one_blocker_names_its_others() {
        let g = [
            n(1, &[RUNNING], &[]),
            n(2, &[READY], &[1]),
            n(3, &[READY], &[1]),
            n(4, &[READY], &[2, 3]),
        ];
        let s = analyze(&g, &Plan::default());
        assert_eq!(nums(&s[0]), [2, 4, 3]);
        assert_eq!(s[0].tree[1].with, [IssueRef::new("acme/app", 3)]);
    }

    #[test]
    fn done_blockers_hold_nothing_back() {
        let mut done = n(1, &[], &[]);
        done.done = true;
        let g = [done, n(2, &[READY], &[1])];
        assert!(analyze(&g, &Plan::default()).is_empty());
        assert!(!waits(&g, &IssueRef::new("acme/app", 2)));
    }

    #[test]
    fn an_issue_waiting_on_two_ends_names_the_other() {
        let g = [
            n(1, &[RUNNING], &[]),
            n(2, &[NEEDS_INPUT], &[]),
            n(3, &[READY], &[1, 2]),
        ];
        let s = analyze(&g, &Plan::default());
        assert_eq!(s[0].at.number, 2);
        assert_eq!(s[0].tree[0].also, [IssueRef::new("acme/app", 1)]);
        assert_eq!(s[1].tree[0].also, [IssueRef::new("acme/app", 2)]);
    }

    #[test]
    fn a_ready_blocker_that_waits_for_a_slot_still_flows() {
        let g = [n(1, &[READY], &[]), n(2, &[READY], &[1])];
        let plan = Plan {
            entries: vec![crate::scheduler::Entry {
                issue: IssueRef::new("acme/app", 1),
                title: "t1".into(),
                epic: None,
                verdict: Verdict::Waiting("no free slot".into()),
            }],
            ..Plan::default()
        };
        let s = analyze(&g, &plan);
        assert_eq!(s[0].end, End::Queued(Some("no free slot".into())));
        assert!(s[0].end.flows());
    }
}
