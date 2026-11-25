//! issuewright's own health: alarms on errors that repeat poll after poll,
//! backoff for issues whose sessions fail to start, and a guard that holds
//! new sessions while the disk is nearly full or the host is overloaded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// An error that showed up in consecutive polls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alarm {
    /// The latest occurrence, as logged.
    pub message: String,
    /// How many log lines of this kind the latest poll had, such as one per
    /// issue it failed for.
    pub lines: usize,
    pub since: DateTime<Utc>,
    pub polls: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// An error repeated for `after` polls.
    Raised(Alarm),
    /// A raised alarm's error didn't come back in the latest poll.
    Cleared(Alarm),
}

/// Errors by kind, across polls.
#[derive(Debug, Default)]
pub struct Alarms {
    open: BTreeMap<String, (Alarm, bool)>,
}

impl Alarms {
    /// Takes the errors of one poll. An error kind seen in `after`
    /// consecutive polls raises an alarm, once; it clears in the first poll
    /// without it.
    pub fn observe(&mut self, errors: &[String], now: DateTime<Utc>, after: u32) -> Vec<Event> {
        let mut seen: BTreeMap<String, (&String, usize)> = BTreeMap::new();
        for e in errors {
            let s = seen.entry(signature(e)).or_insert((e, 0));
            s.0 = e;
            s.1 += 1;
        }
        let mut events = Vec::new();
        self.open.retain(|k, (a, raised)| {
            let keep = seen.contains_key(k);
            if !keep && *raised {
                events.push(Event::Cleared(a.clone()));
            }
            keep
        });
        for (k, (msg, lines)) in seen {
            let (a, raised) = self.open.entry(k).or_insert((
                Alarm {
                    message: String::new(),
                    lines: 0,
                    since: now,
                    polls: 0,
                },
                false,
            ));
            a.message = msg.clone();
            a.lines = lines;
            a.polls += 1;
            if !*raised && a.polls >= after.max(1) {
                *raised = true;
                events.push(Event::Raised(a.clone()));
            }
        }
        events
    }

    /// The raised alarms, oldest first.
    pub fn active(&self) -> Vec<Alarm> {
        let mut v: Vec<Alarm> = self
            .open
            .values()
            .filter(|(_, raised)| *raised)
            .map(|(a, _)| a.clone())
            .collect();
        v.sort_by_key(|a| a.since);
        v
    }
}

impl Alarm {
    /// One line for a notification: how long, and the message.
    pub fn summary(&self, now: DateTime<Utc>) -> String {
        let mins = (now - self.since).num_minutes();
        let more = if self.lines > 1 {
            format!(" ({} like it)", self.lines)
        } else {
            String::new()
        };
        format!("failing for {mins} min{more}: {}", self.message)
    }
}

/// An error's kind: the message with numbers and commit hashes replaced, so
/// the same failure for different issues, slots or commits is one kind.
pub fn signature(msg: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |w: &mut String, out: &mut String| {
        if w.is_empty() {
            return;
        }
        let hexish = w.len() >= 7
            && w.chars().all(|c| c.is_ascii_hexdigit())
            && w.chars().any(|c| c.is_ascii_digit());
        if hexish {
            out.push('H');
        } else {
            let mut digits = false;
            for c in w.chars() {
                if c.is_ascii_digit() {
                    if !digits {
                        out.push('N');
                    }
                    digits = true;
                } else {
                    out.push(c);
                    digits = false;
                }
            }
        }
        w.clear();
    };
    for c in msg.chars() {
        if c.is_ascii_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Consecutive failures to start a session, per issue.
#[derive(Debug)]
pub struct Backoff<K> {
    failures: BTreeMap<K, (u32, DateTime<Utc>)>,
}

impl<K> Default for Backoff<K> {
    fn default() -> Self {
        Self {
            failures: BTreeMap::new(),
        }
    }
}

/// The longest wait between two attempts.
const MAX_BACKOFF_MINUTES: i64 = 30;

impl<K: Ord + Clone> Backoff<K> {
    /// Records a failure; returns when the next attempt is due.
    pub fn fail(&mut self, key: &K, now: DateTime<Utc>) -> DateTime<Utc> {
        let n = self.failures.get(key).map_or(0, |f| f.0) + 1;
        // The first retry is the next poll: most failures are a lock or a
        // network hiccup that is gone by then. Then 2, 4, 8, ... minutes.
        let wait = if n <= 1 {
            Duration::zero()
        } else {
            Duration::minutes((1i64 << (n - 1).min(10)).min(MAX_BACKOFF_MINUTES))
        };
        let next = now + wait;
        self.failures.insert(key.clone(), (n, next));
        next
    }

    pub fn ok(&mut self, key: &K) {
        self.failures.remove(key);
    }

    /// While the issue waits for its next attempt: the failures so far and
    /// when it is due.
    pub fn waiting(&self, key: &K, now: DateTime<Utc>) -> Option<(u32, DateTime<Utc>)> {
        self.failures.get(key).copied().filter(|(_, t)| *t > now)
    }

    /// The issues that wait for their next attempt at `now`.
    pub fn all_waiting(&self, now: DateTime<Utc>) -> Vec<(K, u32, DateTime<Utc>)> {
        self.failures
            .iter()
            .filter(|(_, (_, t))| *t > now)
            .map(|(k, (n, t))| (k.clone(), *n, *t))
            .collect()
    }
}

/// Why new sessions should wait for disk space, if they should: a disk with
/// less than `min_free_gb` free.
pub fn disk_hold(paths: &[PathBuf], min_free_gb: u64) -> Option<String> {
    if min_free_gb == 0 {
        return None;
    }
    paths.iter().find_map(|p| {
        let free = free_bytes(p)?;
        (free < min_free_gb.saturating_mul(1 << 30)).then(|| {
            format!(
                "{} has {} GB free, min_free_disk_gb is {min_free_gb}",
                p.display(),
                free >> 30
            )
        })
    })
}

/// Why new sessions should wait for CPU, if they should: a five-minute load
/// average above `max_load`.
pub fn load_hold(max_load: Option<f64>) -> Option<String> {
    let max = max_load?;
    let load = load_average()?;
    (load > max).then(|| format!("load average {load:.0}, max_load is {max:.0}"))
}

/// Free bytes on the file system of `p` (POSIX `df -Pk`), `None` when
/// unknown.
fn free_bytes(p: &Path) -> Option<u64> {
    let out = Command::new("df").arg("-Pk").arg(p).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// The "Available" column (KiB) of `df -Pk`'s one line, in bytes.
fn parse_df(s: &str) -> Option<u64> {
    let kib: u64 = s.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// The five-minute load average.
fn load_average() -> Option<f64> {
    let mut avg = [0f64; 3];
    // SAFETY: the buffer holds the three values asked for.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    (n >= 2).then_some(avg[1])
}

/// Runs the alarm command with the message as `{message}` (quotes and shell
/// characters removed) and in `ISSUEWRIGHT_ALARM` (verbatim).
pub fn notify(command: &str, message: &str) -> std::io::Result<()> {
    let safe: String = message
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`' | '$' | '\\'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    Command::new("sh")
        .arg("-c")
        .arg(command.replace("{message}", &safe))
        .env("ISSUEWRIGHT_ALARM", message)
        .status()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::minutes(min)
    }

    #[test]
    fn signatures_ignore_issues_and_commits() {
        assert_eq!(
            signature("acme/app#3516: claim failed: Permission denied (os error 13)"),
            signature("acme/app#3404: claim failed: Permission denied (os error 13)")
        );
        assert_eq!(
            signature("slot 3: HEAD at 79f6ce0e836a5 locked"),
            "slot N: HEAD at H locked"
        );
        assert_ne!(
            signature("x#1: claim failed: Permission denied"),
            signature("x#1: checking CI failed: Permission denied")
        );
    }

    #[test]
    fn alarm_raises_after_n_polls_once_and_clears() {
        let mut a = Alarms::default();
        let err = |n: u32| {
            vec![
                format!("o/r#{n}: claim failed: denied"),
                format!("o/r#{}: claim failed: denied", n + 1),
            ]
        };
        assert!(a.observe(&err(1), t(0), 3).is_empty());
        assert!(a.observe(&err(5), t(1), 3).is_empty());
        let ev = a.observe(&err(9), t(2), 3);
        assert_eq!(
            ev,
            [Event::Raised(Alarm {
                message: "o/r#10: claim failed: denied".into(),
                lines: 2,
                since: t(0),
                polls: 3
            })]
        );
        assert!(a.observe(&err(1), t(3), 3).is_empty());
        assert_eq!(a.active().len(), 1);
        assert!(matches!(&a.observe(&[], t(4), 3)[..], [Event::Cleared(_)]));
        assert!(a.active().is_empty());
        // A gap starts the count over.
        a.observe(&err(1), t(5), 3);
        a.observe(&[], t(6), 3);
        a.observe(&err(1), t(7), 3);
        assert!(a.observe(&err(1), t(8), 3).is_empty());
    }

    #[test]
    fn backoff_doubles_up_to_half_an_hour() {
        let mut b = Backoff::<&str>::default();
        assert_eq!(b.fail(&"k", t(0)), t(0));
        assert_eq!(b.waiting(&"k", t(0)), None);
        assert_eq!(b.fail(&"k", t(1)), t(3));
        assert_eq!(b.waiting(&"k", t(2)), Some((2, t(3))));
        assert_eq!(b.fail(&"k", t(3)), t(7));
        for _ in 0..10 {
            b.fail(&"k", t(7));
        }
        assert_eq!(b.waiting(&"k", t(7)).unwrap().1, t(37));
        b.ok(&"k");
        assert_eq!(b.waiting(&"k", t(7)), None);
    }

    #[test]
    fn load_and_disk() {
        assert_eq!(
            parse_df(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 40 60 40% /\n"
            ),
            Some(60 * 1024)
        );
        assert!(load_average().is_some_and(|l| l >= 0.0));
        assert!(free_bytes(Path::new("/")).is_some());
        assert!(disk_hold(&[PathBuf::from("/")], 0).is_none());
        assert!(load_hold(None).is_none());
        assert!(load_hold(Some(f64::MAX)).is_none());
        let held = disk_hold(&[PathBuf::from("/")], 1_000_000).unwrap();
        assert!(held.starts_with("/ has "), "{held}");
    }
}
