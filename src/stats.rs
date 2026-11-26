//! Throughput and efficiency over time, from what issuewright already keeps:
//! its log (every claim, CI result, verdict and handback, with its time) and
//! the slots' Claude Code transcripts (the tokens of every model turn).
//!
//! `issuewright stats` prints it, and `issuewright watch` shows it in its Stats tab.
//! Neither calls GitHub, so merges after auto-merge and the time a PR waited
//! for a human aren't known here; lead time ends at the approval.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;

use crate::config::Config;
use crate::graph::Backlog;
use crate::state::{BacklogSample, State};

/// What one log line says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An implementing session started on the issue.
    Claimed,
    /// A reviewer session started on the issue's PR.
    ReviewStarted,
    /// The session pushed its PR and handed it to CI.
    ToCi,
    CiGreen,
    /// CI failed and issuewright resumed the session to fix it.
    CiRed,
    /// The approved PR conflicted and issuewright resumed the session to rebase.
    Conflict,
    Approved,
    /// The reviewer asked for changes.
    ChangesRequested,
    /// The issue went back to you: a question, a handback. A timeout's
    /// handoff counts when the session then sets `dispatch:needs-input`.
    ToYou,
    Closed,
    /// Something issuewright tried failed.
    Error,
}

/// The issue a log line is about, as `<repo-name>#<n>` without the owner or
/// the session suffix (`-review`, `-epic`), and what happened.
pub fn classify(line: &str) -> Option<(String, Kind)> {
    let (subject, rest) = line.split_once(": ")?;
    let issue = issue_key(subject)?;
    let kind = if rest.starts_with("claimed, slot") {
        Kind::Claimed
    } else if rest.starts_with("reviewing ") {
        Kind::ReviewStarted
    } else if rest.starts_with("finished (its issue is dispatch:ci)") {
        Kind::ToCi
    } else if rest.starts_with("CI is green on") || rest.starts_with("No checks reported on") {
        Kind::CiGreen
    } else if rest.starts_with("CI failed on") && rest.contains("resumed") {
        Kind::CiRed
    } else if rest.contains("conflicts with the default branch, resumed") {
        Kind::Conflict
    } else if rest.starts_with("approved, auto-merge enabled")
        || rest.starts_with("approved, merged")
    {
        Kind::Approved
    } else if rest.starts_with("finished (it handed the PR back)") {
        Kind::ChangesRequested
    } else if rest.starts_with("finished (its issue is closed)")
        || rest.starts_with("its issue is closed,")
    {
        Kind::Closed
    } else if rest.contains("failed: ") || rest.starts_with("couldn't ") {
        Kind::Error
    } else if rest.starts_with("finished (its issue is dispatch:needs-input)")
        || rest.contains("needs input")
    {
        Kind::ToYou
    } else {
        return None;
    };
    Some((issue, kind))
}

fn issue_key(subject: &str) -> Option<String> {
    let s = subject.rsplit('/').next()?;
    let (name, rest) = s.split_once('#')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if name.is_empty() || name.contains(' ') || digits.is_empty() {
        return None;
    }
    Some(format!("{name}#{digits}"))
}

/// One day's numbers, in local time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Day {
    pub date: NaiveDate,
    pub claimed: usize,
    pub reviews: usize,
    pub to_ci: usize,
    pub ci_red: usize,
    pub conflicts: usize,
    pub approved: usize,
    pub changes_requested: usize,
    pub to_you: usize,
    pub closed: usize,
    pub errors: usize,
    /// From an issue's first claim to its approval, for approvals this day.
    pub lead_minutes: Vec<i64>,
    /// From handing a PR to CI to its result, for results this day.
    pub ci_minutes: Vec<i64>,
    pub tokens: Tokens,
    /// The backlog at the day's last poll, once issuewright recorded it.
    pub backlog: Option<Backlog>,
}

impl Day {
    pub fn lead_p50(&self) -> Option<i64> {
        percentile(&self.lead_minutes, 50)
    }
    pub fn ci_p50(&self) -> Option<i64> {
        percentile(&self.ci_minutes, 50)
    }
    /// Output tokens per approved PR.
    pub fn output_per_approval(&self) -> Option<u64> {
        (self.approved > 0 && self.tokens.output > 0)
            .then(|| self.tokens.output / self.approved as u64)
    }
}

pub fn percentile(xs: &[i64], p: usize) -> Option<i64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some(v[((v.len() - 1) * p).div_ceil(100)])
}

/// Tokens of model turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// Everything the model read: fresh input and the cache, written or read.
    pub fn read(&self) -> u64 {
        self.input + self.cache_write + self.cache_read
    }

    fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.output += o.output;
    }
}

/// Days from the log's lines (time, message), oldest first, every day from
/// the first event to `today` included.
pub fn days(lines: &[(DateTime<Utc>, String)], today: NaiveDate) -> Vec<Day> {
    let mut by_day: BTreeMap<NaiveDate, Day> = BTreeMap::new();
    let mut first_claim: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut approved: HashSet<String> = HashSet::new();
    let mut in_ci: HashMap<String, DateTime<Utc>> = HashMap::new();
    for (t, l) in lines {
        let Some((issue, kind)) = classify(l) else {
            continue;
        };
        let date = t.with_timezone(&Local).date_naive();
        let d = by_day.entry(date).or_insert_with(|| Day {
            date,
            ..Day::default()
        });
        match kind {
            Kind::Claimed => {
                d.claimed += 1;
                first_claim.entry(issue).or_insert(*t);
            }
            Kind::ReviewStarted => d.reviews += 1,
            Kind::ToCi => {
                d.to_ci += 1;
                in_ci.insert(issue, *t);
            }
            Kind::CiGreen | Kind::CiRed => {
                if kind == Kind::CiRed {
                    d.ci_red += 1;
                }
                if let Some(since) = in_ci.remove(&issue) {
                    d.ci_minutes.push((*t - since).num_minutes());
                }
            }
            Kind::Conflict => d.conflicts += 1,
            Kind::Approved => {
                d.approved += 1;
                if approved.insert(issue.clone())
                    && let Some(c) = first_claim.get(&issue)
                {
                    d.lead_minutes.push((*t - *c).num_minutes());
                }
            }
            Kind::ChangesRequested => d.changes_requested += 1,
            Kind::ToYou => d.to_you += 1,
            Kind::Closed => d.closed += 1,
            Kind::Error => d.errors += 1,
        }
    }
    let Some(&first) = by_day.keys().next() else {
        return vec![];
    };
    first
        .iter_days()
        .take_while(|d| *d <= today)
        .map(|date| {
            by_day.remove(&date).unwrap_or(Day {
                date,
                ..Day::default()
            })
        })
        .collect()
}

/// Adds the tokens of each local day to `days`.
pub fn add_tokens(days: &mut [Day], tokens: &BTreeMap<NaiveDate, Tokens>) {
    for d in days {
        if let Some(t) = tokens.get(&d.date) {
            d.tokens.add(t);
        }
    }
}

/// Gives each day the backlog of its last sample, in local time.
pub fn add_backlog(days: &mut [Day], history: &[BacklogSample]) {
    for d in days {
        d.backlog = history
            .iter()
            .rfind(|s| s.at.with_timezone(&Local).date_naive() == d.date)
            .map(|s| s.counts);
    }
}

/// The whole log, parsed.
pub fn read_log(cfg: &Config) -> Vec<(DateTime<Utc>, String)> {
    let text = std::fs::read_to_string(cfg.logs_dir().join("issuewright.log")).unwrap_or_default();
    parse_log(&text)
}

/// Lines with their time, skipping lines without one (the continuation of a
/// multi-line message).
pub fn parse_log(text: &str) -> Vec<(DateTime<Utc>, String)> {
    text.lines()
        .filter_map(|l| {
            let (t, rest) = l.split_once(' ')?;
            let t = DateTime::parse_from_rfc3339(t).ok()?.with_timezone(&Utc);
            Some((t, rest.to_string()))
        })
        .collect()
}

/// Reads the slots' transcripts and sums their tokens per local day. Kept
/// between calls, it reads only what was appended since. A transcript issuewright
/// copied into another slot to resume it repeats the turns before the copy,
/// and a turn is written once per content block: each message id counts
/// once, with the highest numbers any of its lines show.
#[derive(Default)]
pub struct TokenScan {
    files: HashMap<PathBuf, u64>,
    /// What each message id counted so far, per session (the transcript's
    /// name). A turn with several content blocks is written once per block.
    seen: HashMap<String, HashMap<String, Tokens>>,
    pub by_day: BTreeMap<NaiveDate, Tokens>,
}

impl TokenScan {
    /// Reads what's new in every transcript of `cfg`'s slots.
    pub fn update(&mut self, cfg: &Config) {
        for f in transcripts(cfg) {
            self.read(&f);
        }
    }

    fn read(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let from = self.files.get(path).copied().unwrap_or(0);
        if len <= from || file.seek(SeekFrom::Start(from)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return;
        }
        // A line still being written ends without a newline: read it next time.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
            return;
        };
        self.files.insert(path.to_path_buf(), from + end as u64 + 1);
        let session = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let seen = self.seen.entry(session).or_default();
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            if let Some((date, id, t)) = assistant_turn(line) {
                let had = seen.entry(id).or_default();
                let more = Tokens {
                    input: t.input.saturating_sub(had.input),
                    cache_write: t.cache_write.saturating_sub(had.cache_write),
                    cache_read: t.cache_read.saturating_sub(had.cache_read),
                    output: t.output.saturating_sub(had.output),
                };
                had.add(&more);
                self.by_day.entry(date).or_default().add(&more);
            }
        }
    }
}

/// A model turn's local day, message id and tokens.
fn assistant_turn(line: &str) -> Option<(NaiveDate, String, Tokens)> {
    if !line.contains(r#""type":"assistant""#) {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let m = v.get("message")?;
    if m.get("model").and_then(Value::as_str) == Some("<synthetic>") {
        return None;
    }
    let u = m.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let t: DateTime<Utc> = v.get("timestamp")?.as_str()?.parse().ok()?;
    Some((
        t.with_timezone(&Local).date_naive(),
        m.get("id")?.as_str()?.to_string(),
        Tokens {
            input: n("input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            output: n("output_tokens"),
        },
    ))
}

/// Transcript files of every slot: Claude Code keeps them per working
/// directory, under a name made from its path.
fn transcripts(cfg: &Config) -> Vec<PathBuf> {
    let probe = crate::session::transcript_dir(&cfg.slots_dir());
    let (Some(root), Some(prefix)) = (probe.parent(), probe.file_name()) else {
        return vec![];
    };
    let prefix = prefix.to_string_lossy().into_owned();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut out = Vec::new();
    for d in dirs.flatten() {
        if !d.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(d.path()) {
            out.extend(
                files
                    .flatten()
                    .map(|f| f.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "jsonl")),
            );
        }
    }
    out
}

/// `1.2M`, `340k`, `12`.
pub fn human(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.0}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

/// `45m`, `3h10`, `2d4h`.
pub fn duration(mins: i64) -> String {
    match mins {
        ..60 => format!("{mins}m"),
        60..1440 => format!("{}h{:02}", mins / 60, mins % 60),
        _ => format!("{}d{}h", mins / 1440, mins % 1440 / 60),
    }
}

/// A table: its header and one row per day, newest first.
pub type Table = (Vec<&'static str>, Vec<Vec<String>>);

/// The tables `issuewright stats` prints and the Stats tab shows: what got
/// done, and how long it took, what it cost and what was left.
pub fn tables(days: &[Day]) -> [Table; 2] {
    let opt = |o: Option<String>| o.unwrap_or_else(|| "–".into());
    let day = |d: &Day| d.date.format("%a %m-%d").to_string();
    let work = (
        vec![
            "day", "started", "reviews", "to CI", "CI red", "rebase", "changes", "approved",
            "closed", "to you", "errors",
        ],
        days.iter()
            .rev()
            .map(|d| {
                let mut r = vec![day(d)];
                r.extend(
                    [
                        d.claimed,
                        d.reviews,
                        d.to_ci,
                        d.ci_red,
                        d.conflicts,
                        d.changes_requested,
                        d.approved,
                        d.closed,
                        d.to_you,
                        d.errors,
                    ]
                    .map(|n| n.to_string()),
                );
                r
            })
            .collect(),
    );
    let tokens = |n: u64| if n > 0 { human(n) } else { "–".into() };
    let flow = (
        vec![
            "day",
            "lead p50",
            "CI p50",
            "read",
            "output",
            "out/approved",
            "qualified",
            "behind",
            "held",
            "needs you",
            "parked",
            "in flight",
        ],
        days.iter()
            .rev()
            .map(|d| {
                let mut r = vec![
                    day(d),
                    opt(d.lead_p50().map(duration)),
                    opt(d.ci_p50().map(duration)),
                    tokens(d.tokens.read()),
                    tokens(d.tokens.output),
                    opt(d.output_per_approval().map(human)),
                ];
                r.extend(match d.backlog {
                    Some(b) => [
                        b.qualified,
                        b.behind_moving,
                        b.held,
                        b.needs_input,
                        b.parked,
                        b.in_flight,
                    ]
                    .map(|n| n.to_string()),
                    None => std::array::from_fn(|_| "–".to_string()),
                });
                r
            })
            .collect(),
    );
    [work, flow]
}

/// Totals over `days`: approvals, median lead time, CI red rounds per PR
/// handed to CI, handbacks per implementing session (fresh or resumed),
/// output tokens per approval.
pub fn summary(days: &[Day]) -> String {
    let sum = |f: fn(&Day) -> usize| days.iter().map(f).sum::<usize>();
    let (approved, to_ci, ci_red, to_you) = (
        sum(|d| d.approved),
        sum(|d| d.to_ci),
        sum(|d| d.ci_red),
        sum(|d| d.to_you),
    );
    // Implementing sessions: fresh claims and resumes to fix CI or rebase.
    let sessions = sum(|d| d.claimed + d.ci_red + d.conflicts);
    let lead: Vec<i64> = days.iter().flat_map(|d| d.lead_minutes.clone()).collect();
    let ci: Vec<i64> = days.iter().flat_map(|d| d.ci_minutes.clone()).collect();
    let output: u64 = days.iter().map(|d| d.tokens.output).sum();
    let pct = |a: usize, b: usize| {
        (a * 100)
            .checked_div(b)
            .map_or_else(|| "–".to_string(), |p| format!("{p}%"))
    };
    let opt = |o: Option<i64>| o.map(duration).unwrap_or_else(|| "–".into());
    let tokens = if output == 0 {
        String::new()
    } else {
        format!(
            " · {} output tokens, {} per approval",
            human(output),
            if approved > 0 {
                human(output / approved as u64)
            } else {
                "–".into()
            }
        )
    };
    format!(
        "{approved} approved in {} days ({:.1}/day) · lead time p50 {} p90 {} · CI p50 {} p90 {} · CI red {} of PRs · \
         to you {} of sessions{tokens}",
        days.len(),
        approved as f64 / days.len().max(1) as f64,
        opt(percentile(&lead, 50)),
        opt(percentile(&lead, 90)),
        opt(percentile(&ci, 50)),
        opt(percentile(&ci, 90)),
        pct(ci_red, to_ci),
        pct(to_you, sessions),
    )
}

fn print_table(header: &[&str], rows: Vec<Vec<String>>) {
    let width = |i: usize| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .chain([header[i].len()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..header.len()).map(width).collect();
    let line = |cells: Vec<String>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    format!("{c:<w$}", w = widths[i])
                } else {
                    format!("{c:>w$}", w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", line(header.iter().map(|s| s.to_string()).collect()));
    for r in rows {
        println!("{}", line(r));
    }
}

/// `issuewright stats`: the last `days` days.
pub fn print(cfg: &Config, n: usize, tokens: bool) -> Result<()> {
    let today = Local::now().date_naive();
    let mut all = days(&read_log(cfg), today);
    if let Ok(h) = State::open(cfg.state_dir()).and_then(|s| s.backlog()) {
        add_backlog(&mut all, &h);
    }
    if tokens {
        let mut scan = TokenScan::default();
        scan.update(cfg);
        add_tokens(&mut all, &scan.by_day);
    }
    let last = &all[all.len().saturating_sub(n)..];
    if last.is_empty() {
        println!("No events in issuewright's log yet.");
        return Ok(());
    }
    for (i, (header, rows)) in tables(last).into_iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_table(&header, rows);
    }
    println!("\n{}", summary(last));
    println!(
        "\nlead: first claim to approval. CI: handed to CI to green or red, queue included. read, output: model \
         tokens of the slots' sessions{}.\nBacklog at the day's last poll: qualified (ready, nothing in the way), \
         behind (waits only on work in flight), held (waits on something only you can move), needs you \
         (dispatch:needs-input), parked (status:blocked), in flight (running, CI, review, merging).",
        if tokens {
            ""
        } else {
            " (skipped, --no-tokens)"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const LOG: &str = "\
2026-11-22T08:00:00Z app#1: claimed, slot 1, window @1, branch feat/1-x
2026-11-22T08:30:00Z app#1: finished (its issue is dispatch:ci), the session exits
2026-11-22T09:10:00Z app#1: CI failed on acme/app#2 at abc1234, resumed in slot 1, window @2
2026-11-22T09:20:00Z app#1: finished (its issue is dispatch:ci), the session exits
2026-11-22T10:20:00Z acme/app#1: CI is green on acme/app#2, dispatch:review
2026-11-22T10:21:00Z app#1-review: reviewing acme/app#2 at abc, slot 2, window @3
2026-11-22T11:00:00Z app#1-review: approved, merged acme/app#2
2026-11-22T11:01:00Z acme/app#5: claim failed: Permission denied (os error 13)
2026-11-22T11:02:00Z app#6: finished (its issue is dispatch:needs-input), the session exits
2026-11-22T11:03:00Z app#7: no answer for 15 min, told it to move its question to GitHub
2026-11-22T11:04:00Z app slot 2: removed target/ (74 GB)
";

    #[test]
    fn classifies_the_daemons_lines() {
        let k = classify;
        assert_eq!(
            k("acme/app#3509: checking CI failed: EOF"),
            Some(("app#3509".into(), Kind::Error))
        );
        assert_eq!(
            k("app#3437-review: approved, auto-merge enabled on o/r#3"),
            Some(("app#3437".into(), Kind::Approved))
        );
        assert_eq!(
            k("app#3: approved, but merging failed: merging o/r#4: x"),
            Some(("app#3".into(), Kind::Error))
        );
        assert_eq!(k("app slot 2: removed target/ (74 GB)"), None);
        assert_eq!(k("issuewright started: mode interactive"), None);
    }

    #[test]
    fn days_count_events_and_times() {
        let lines = parse_log(LOG);
        let today = lines[0].0.with_timezone(&Local).date_naive();
        let d = days(&lines, today);
        assert_eq!(d.len(), 1);
        let d = &d[0];
        assert_eq!(
            (
                d.claimed, d.to_ci, d.ci_red, d.reviews, d.approved, d.errors, d.to_you
            ),
            (1, 2, 1, 1, 1, 1, 1)
        );
        // 08:00 claim to 11:00 approval.
        assert_eq!(d.lead_minutes, [180]);
        // 08:30 → 09:10 red, 09:20 → 10:20 green.
        assert_eq!(d.ci_minutes, [40, 60]);
        assert_eq!(d.ci_p50(), Some(60));
    }

    #[test]
    fn empty_days_are_filled_in() {
        let lines = parse_log(LOG);
        let first = lines[0].0.with_timezone(&Local).date_naive();
        let d = days(&lines, first + chrono::Days::new(2));
        assert_eq!(d.len(), 3);
        assert_eq!(d[2].claimed, 0);
        let [work, flow] = tables(&d);
        assert_eq!((work.1.len(), flow.1.len()), (3, 3));
        assert_eq!(flow.1[0].len(), flow.0.len());
        // No backlog recorded and no tokens: dashes, not zeros.
        assert_eq!(flow.1[0][5..], ["–"; 7]);
    }

    #[test]
    fn a_day_takes_the_backlog_of_its_last_sample() {
        let at = |h: u32| {
            Local
                .with_ymd_and_hms(2026, 11, 22, h, 0, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        let sample = |h, q| BacklogSample {
            at: at(h),
            counts: Backlog {
                qualified: q,
                ..Default::default()
            },
        };
        let mut d = vec![
            Day {
                date: at(12).with_timezone(&Local).date_naive(),
                ..Default::default()
            },
            Day {
                date: at(12).with_timezone(&Local).date_naive() + chrono::Days::new(1),
                ..Default::default()
            },
        ];
        add_backlog(&mut d, &[sample(9, 4), sample(20, 7)]);
        assert_eq!(d[0].backlog.map(|b| b.qualified), Some(7));
        assert_eq!(d[1].backlog, None);
    }

    #[test]
    fn tokens_count_each_message_once_across_copies() {
        let root = std::env::temp_dir().join(format!("issuewright-stats-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let turn = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-11-22T10:00:00Z","message":{{"id":"{id}","model":"opus","usage":{{"input_tokens":1,"cache_read_input_tokens":100,"output_tokens":{out}}}}}}}"#
            )
        };
        // The same turn twice (two content blocks), then a copy in another
        // slot that adds one turn, and a line still being written.
        std::fs::write(
            a.join("s.jsonl"),
            format!("{}\n{}\n", turn("m1", 4), turn("m1", 10)),
        )
        .unwrap();
        std::fs::write(
            b.join("s.jsonl"),
            format!("{}\n{}\n{}", turn("m1", 10), turn("m2", 5), turn("m3", 7)),
        )
        .unwrap();
        let mut scan = TokenScan::default();
        scan.read(&a.join("s.jsonl"));
        scan.read(&b.join("s.jsonl"));
        let t = scan.by_day.values().next().copied().unwrap();
        assert_eq!((t.output, t.cache_read, t.input), (15, 200, 2));
        // Reading again adds only what was appended.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(b.join("s.jsonl"))
            .unwrap();
        std::io::Write::write_all(&mut f, b"\n").unwrap();
        scan.read(&b.join("s.jsonl"));
        scan.read(&a.join("s.jsonl"));
        assert_eq!(scan.by_day.values().next().unwrap().output, 22);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(human(950), "950");
        assert_eq!(human(19_004_107), "19.0M");
        assert_eq!(duration(45), "45m");
        assert_eq!(duration(190), "3h10");
        assert_eq!(duration(3000), "2d2h");
        assert_eq!(percentile(&[5, 1, 3], 50), Some(3));
        assert_eq!(percentile(&[], 50), None);
    }
}
