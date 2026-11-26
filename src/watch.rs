//! `issuewright watch`: a screen to keep open beside the daemon. It shows the
//! sessions in the slots, the issues further down the pipeline, and for every
//! ready issue that waits on a blocker, the issue its chain of blockers ends
//! at, with the ends that won't move until you act at the top.
//!
//! Its Stats tab (`4`) shows throughput and efficiency per day, from the
//! daemon's log and the slots' transcripts (see [`crate::stats`]).
//!
//! It reads only what the daemon writes (`status.json`, the slot locks, the
//! log), the transcripts, and asks tmux which windows are alive, so it calls
//! no GitHub API and changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Paragraph, Row, Table, Wrap};

use crate::config::Config;
use crate::graph::{self, End, Stall};
use crate::model::*;
use crate::scheduler::Verdict;
use crate::state::{Kind, SlotLock, State, Status, Waiting};
use crate::stats::{self, Day, Tokens};
use crate::tmux;

/// Sessions waiting for an answer, by lowercase repository and slot.
type WaitingBySlot = BTreeMap<(String, usize), Waiting>;

/// Tokens per local day, once the background thread has read the
/// transcripts.
type SharedTokens = Arc<Mutex<Option<BTreeMap<chrono::NaiveDate, Tokens>>>>;

const REFRESH: Duration = Duration::from_secs(2);
/// Lines of the events pane.
const EVENTS: usize = 8;
/// How much of the end of issuewright's log the screen reads.
const LOG_TAIL: u64 = 1 << 20;

struct View {
    status: Status,
    locks: Vec<SlotLock>,
    windows: Option<BTreeSet<String>>,
    stalls: Vec<Stall>,
    error: Option<String>,
    waiting: WaitingBySlot,
    question_timeout: u64,
    /// issuewright's log lines, oldest first, each with its time.
    log: Vec<(DateTime<Utc>, String)>,
    /// What all configured repository names start with, up to a `-`, left
    /// out when naming issues: `acme-api#12` reads `api#12`.
    prefix: String,
}

impl View {
    fn load(cfg: &Config) -> Self {
        let read = || -> Result<(Status, Vec<SlotLock>, WaitingBySlot)> {
            let state = State::open(cfg.state_dir())?;
            let locks = state.locks()?;
            let waiting = locks
                .iter()
                .filter_map(|l| {
                    let w = state.waiting(&l.repo, l.slot)?;
                    (w.since >= l.started_at).then(|| ((l.repo.to_lowercase(), l.slot), w))
                })
                .collect();
            Ok((state.status()?, locks, waiting))
        };
        let (status, locks, waiting, error) = match read() {
            Ok((s, l, w)) => (s, l, w, None),
            Err(e) => (
                Status::default(),
                vec![],
                BTreeMap::new(),
                Some(format!("{e:#}")),
            ),
        };
        let stalls = graph::analyze(&status.graph, &status.plan);
        Self {
            status,
            locks,
            windows: tmux::window_ids(),
            stalls,
            error,
            waiting,
            question_timeout: cfg.question_timeout_secs,
            log: read_log(&cfg.logs_dir().join("issuewright.log")),
            prefix: common_prefix(cfg.repos.iter().map(|r| r.short())),
        }
    }

    fn repo<'a>(&self, name: &'a str) -> &'a str {
        let name = name.rsplit('/').next().unwrap_or(name);
        match name.strip_prefix(&self.prefix) {
            Some(rest) if !rest.is_empty() => rest,
            _ => name,
        }
    }

    fn name(&self, r: &IssueRef) -> String {
        format!("{}#{}", self.repo(&r.repo), r.number)
    }

    /// `#12` within `to`'s repository, `repo#12` across.
    fn rel(&self, r: &IssueRef, to: &IssueRef) -> String {
        if r.repo.eq_ignore_ascii_case(&to.repo) {
            format!("#{}", r.number)
        } else {
            self.name(r)
        }
    }

    /// The window of a session working on `r`: its own, else its review's.
    fn window_of(&self, r: &IssueRef) -> Option<String> {
        let mut ls: Vec<&SlotLock> = self
            .locks
            .iter()
            .filter(|l| l.issue.key() == r.key())
            .collect();
        ls.sort_by_key(|l| l.kind != Kind::Implement);
        ls.into_iter().find_map(|l| l.tmux_window.clone())
    }

    fn waiting_of(&self, l: &SlotLock) -> Option<&Waiting> {
        self.waiting.get(&(l.repo.to_lowercase(), l.slot))
    }

    /// Log lines from today, local time.
    fn today(&self) -> impl Iterator<Item = &str> {
        let today = Local::now().date_naive();
        self.log
            .iter()
            .filter(move |(t, _)| t.with_timezone(&Local).date_naive() == today)
            .map(|(_, l)| l.as_str())
    }

    /// A log line with the repositories' owners and common prefix left out.
    fn shorten(&self, line: &str) -> String {
        let mut s = line.to_string();
        let owners: BTreeSet<&str> = self
            .locks
            .iter()
            .map(|l| l.repo.as_str())
            .chain(self.status.graph.iter().map(|n| n.issue.repo.as_str()))
            .filter_map(|r| r.split_once('/').map(|(o, _)| o))
            .collect();
        for o in owners {
            s = s.replace(&format!("{o}/"), "");
        }
        if !self.prefix.is_empty() {
            s = s.replace(&self.prefix, "");
        }
        s
    }

    fn names(&self, rs: &[IssueRef], to: &IssueRef) -> String {
        rs.iter()
            .map(|r| self.rel(r, to))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The log's last lines with their time, skipping lines without one (the
/// continuation of a multi-line message).
fn read_log(path: &std::path::Path) -> Vec<(DateTime<Utc>, String)> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    parse_log(&String::from_utf8_lossy(&buf))
}

fn parse_log(text: &str) -> Vec<(DateTime<Utc>, String)> {
    stats::parse_log(text)
}

fn common_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut p = first.len();
    for n in &names[1..] {
        p = p.min(
            first
                .bytes()
                .zip(n.bytes())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    match first[..p].rfind('-') {
        Some(i) if names.len() > 1 => first[..=i].to_string(),
        _ => String::new(),
    }
}

pub fn run(cfg: &Config) -> Result<()> {
    let mut term = ratatui::init();
    let res = run_loop(cfg, &mut term);
    ratatui::restore();
    res
}

/// Something a line points at: an issue, and the window of its session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    issue: IssueRef,
    window: Option<String>,
}

/// A pane's lines, and what each one points at.
#[derive(Default)]
struct Rows {
    lines: Vec<Line<'static>>,
    targets: Vec<Option<Target>>,
}

impl Rows {
    fn push(&mut self, line: Line<'static>, target: Option<Target>) {
        self.lines.push(line);
        self.targets.push(target);
    }

    fn index_of(&self, t: &IssueRef) -> Option<usize> {
        self.targets
            .iter()
            .position(|x| x.as_ref().is_some_and(|x| x.issue.key() == t.key()))
    }

    fn first(&self) -> Option<usize> {
        self.targets.iter().position(Option::is_some)
    }

    /// The line with a target `by` such lines away from line `from`
    /// (negative: up), stopping at the first and the last.
    fn step(&self, from: usize, by: isize) -> Option<usize> {
        let picks: Vec<usize> = (0..self.targets.len())
            .filter(|i| self.targets[*i].is_some())
            .collect();
        let at = picks.iter().position(|i| *i == from)? as isize;
        let to = (at + by).clamp(0, picks.len() as isize - 1);
        picks.get(to as usize).copied()
    }
}

const PANES: [&str; 3] = ["Sessions", "Pipeline", "Blockers"];

#[derive(Default)]
struct App {
    /// The Stats tab instead of the live panes.
    stats_tab: bool,
    focus: usize,
    /// The selected issue of each pane.
    sel: [Option<IssueRef>; 3],
    scroll: [u16; 3],
    /// Visible lines of each pane at the last draw.
    height: [u16; 3],
    message: Option<(String, Instant)>,
}

impl App {
    fn selected(&self, rows: &Rows, pane: usize) -> Option<usize> {
        self.sel[pane]
            .as_ref()
            .and_then(|r| rows.index_of(r))
            .or_else(|| rows.first())
    }

    fn select(&mut self, rows: &Rows, pane: usize, by: isize) {
        let Some(at) = self.selected(rows, pane) else {
            return;
        };
        if let Some(i) = rows.step(at, by) {
            self.sel[pane] = rows.targets[i].as_ref().map(|t| t.issue.clone());
        }
    }

    fn say(&mut self, m: impl Into<String>) {
        self.message = Some((m.into(), Instant::now()));
    }
}

fn run_loop(cfg: &Config, term: &mut DefaultTerminal) -> Result<()> {
    let mut view = View::load(cfg);
    let mut loaded = Instant::now();
    let mut app = App::default();
    let mut stats = StatsTab::default();
    loop {
        let panes = [
            session_rows(cfg, &view),
            pipeline_rows(&view),
            dependency_rows(&view),
        ];
        if app.stats_tab {
            stats.refresh(cfg);
            term.draw(|f| draw_stats(f, &stats, &app))?;
        } else {
            term.draw(|f| draw(f, cfg, &view, &panes, &mut app))?;
        }
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
        {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let p = app.focus;
            let page = app.height[p].max(2) as isize - 1;
            if app.stats_tab {
                match k.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char(c @ '1'..='3') => {
                        app.stats_tab = false;
                        app.focus = c as usize - '1' as usize;
                    }
                    KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('4') => {
                        app.stats_tab = false
                    }
                    KeyCode::Char('r') => stats.loaded = None,
                    _ => {}
                }
                continue;
            }
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Tab => app.focus = (p + 1) % PANES.len(),
                KeyCode::BackTab => app.focus = (p + PANES.len() - 1) % PANES.len(),
                KeyCode::Char(c @ '1'..='3') => app.focus = c as usize - '1' as usize,
                KeyCode::Char('4') => app.stats_tab = true,
                KeyCode::Down | KeyCode::Char('j') => app.select(&panes[p], p, 1),
                KeyCode::Up | KeyCode::Char('k') => app.select(&panes[p], p, -1),
                KeyCode::PageDown | KeyCode::Char(' ') => app.select(&panes[p], p, page),
                KeyCode::PageUp => app.select(&panes[p], p, -page),
                KeyCode::Home | KeyCode::Char('g') => app.select(&panes[p], p, -10_000),
                KeyCode::End | KeyCode::Char('G') => app.select(&panes[p], p, 10_000),
                KeyCode::Enter => {
                    let t = app
                        .selected(&panes[p], p)
                        .and_then(|i| panes[p].targets[i].clone());
                    match t {
                        None => {}
                        Some(t) => match t.window.or_else(|| view.window_of(&t.issue)) {
                            None => app.say(format!("{} has no session", view.name(&t.issue))),
                            Some(w) => match tmux::switch_to(&w) {
                                Ok(()) => app.say(format!(
                                    "switched to {}; come back with prefix + L",
                                    view.name(&t.issue)
                                )),
                                Err(e) => app.say(format!(
                                    "{e:#}: attach with `tmux attach -t {}`",
                                    cfg.tmux_session
                                )),
                            },
                        },
                    }
                }
                KeyCode::Char('o') => {
                    if let Some(t) = app
                        .selected(&panes[p], p)
                        .and_then(|i| panes[p].targets[i].clone())
                    {
                        let url = format!(
                            "https://github.com/{}/issues/{}",
                            t.issue.repo, t.issue.number
                        );
                        match open_url(&url) {
                            Ok(()) => app.say(format!("opened {url}")),
                            Err(e) => app.say(format!("{e:#}: {url}")),
                        }
                    }
                }
                KeyCode::Char('r') => {
                    view = View::load(cfg);
                    loaded = Instant::now();
                }
                _ => {}
            }
        }
        if loaded.elapsed() >= REFRESH {
            view = View::load(cfg);
            loaded = Instant::now();
        }
    }
}

fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("running {opener}"))?;
    Ok(())
}

fn draw(f: &mut Frame, cfg: &Config, v: &View, panes: &[Rows; 3], app: &mut App) {
    let slots: usize = cfg.repos.iter().map(|r| r.slots).sum();
    let top = (panes[0].lines.len().max(panes[1].lines.len()).max(1) as u16 + 2).min(14);
    let header = header_lines(cfg, v, slots);
    let [head, upper, deps, events, footer] = Layout::vertical([
        Constraint::Length(header.len() as u16),
        Constraint::Length(top),
        Constraint::Min(5),
        Constraint::Length(EVENTS as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [sessions, pipe] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(upper);

    f.render_widget(Paragraph::new(header), head);
    let stuck = v.stalls.iter().filter(|s| !s.end.flows()).count();
    let blockers_title = if stuck > 0 {
        vec![
            " 3 Blockers ".into(),
            Span::styled(
                format!(" {stuck} stuck "),
                Style::new().white().on_red().bold(),
            ),
            " ".into(),
        ]
    } else {
        vec![" 3 Blockers ".into()]
    };
    let titles = [
        vec![" 1 Sessions ".into()],
        vec![" 2 Pipeline ".into()],
        blockers_title,
    ];
    for (i, area) in [sessions, pipe, deps].into_iter().enumerate() {
        pane(f, area, titles[i].clone(), &panes[i], app, i);
    }
    f.render_widget(
        Paragraph::new(event_lines(v))
            .block(Block::bordered().title(" Events ").border_style(dim())),
        events,
    );
    footer_line(f, footer, app);
}

/// Draws one selectable pane, keeping its selected line in view.
fn pane(
    f: &mut Frame,
    area: Rect,
    title: Vec<Span<'static>>,
    rows: &Rows,
    app: &mut App,
    i: usize,
) {
    let focused = app.focus == i;
    let inner = area.height.saturating_sub(2);
    app.height[i] = inner;
    let sel = app.selected(rows, i);
    if let Some(at) = sel {
        let at = at as u16;
        if at < app.scroll[i] {
            app.scroll[i] = at;
        } else if inner > 0 && at >= app.scroll[i] + inner {
            app.scroll[i] = at + 1 - inner;
        }
    }
    let max = (rows.lines.len() as u16).saturating_sub(inner);
    app.scroll[i] = app.scroll[i].min(max);
    let mut lines = rows.lines.clone();
    if focused && let Some(at) = sel {
        lines[at] = lines[at].clone().patch_style(
            Style::new()
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::BOLD),
        );
    }
    let border = if focused { Style::new().cyan() } else { dim() };
    f.render_widget(
        Paragraph::new(lines).scroll((app.scroll[i], 0)).block(
            Block::bordered()
                .title(Line::from(title))
                .border_style(border),
        ),
        area,
    );
}

fn ago(t: DateTime<Utc>) -> String {
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn header_lines(cfg: &Config, v: &View, slots: usize) -> Vec<Line<'static>> {
    let mut spans = vec![
        Span::styled("issuewright", Style::new().bold()),
        "  ".into(),
    ];
    match v.status.last_poll {
        Some(t) => {
            let late = (Utc::now() - t).num_seconds() > (cfg.poll_interval_secs * 3 + 30) as i64;
            let style = if late {
                Style::new().red().bold()
            } else {
                Style::new()
            };
            spans.push(Span::styled(format!("last poll {} ago", ago(t)), style));
            if late {
                spans.push(Span::styled(
                    " (is issuewright running?)",
                    Style::new().red(),
                ));
            }
            if let Some(m) = v.status.mode {
                spans.push(Span::styled(format!(" · {m}"), dim()));
            }
        }
        None => spans.push(Span::styled("never polled", Style::new().red())),
    }
    let waiting = v
        .status
        .plan
        .entries
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Waiting(_)))
        .count();
    let stuck = v.stalls.iter().filter(|s| !s.end.flows()).count();
    let asking = v.waiting.len();
    spans.push(format!("  ·  {}/{slots} slots  ·  {waiting} waiting", v.locks.len()).into());
    if asking > 0 {
        spans.push("  ·  ".into());
        spans.push(Span::styled(
            format!("{asking} asking you"),
            Style::new().yellow().bold(),
        ));
    }
    if stuck > 0 {
        spans.push("  ·  ".into());
        spans.push(Span::styled(
            format!("{stuck} stuck"),
            Style::new().red().bold(),
        ));
    }
    let mut out = vec![
        Line::from(spans),
        today_line(v, cfg.max_sessions.min(slots)),
    ];
    if let Some(e) = v.error.as_ref().or(v.status.last_error.as_ref()) {
        out.push(Line::styled(format!("error: {e}"), Style::new().red()));
    }
    if let Some(h) = &v.status.hold {
        out.push(Line::styled(h.clone(), Style::new().yellow().bold()));
    }
    for a in v.status.alarms.iter().take(3) {
        out.push(Line::styled(
            format!("alarm: {}", a.summary(Utc::now())),
            Style::new().red().bold(),
        ));
    }
    out
}

/// What happened today, counted from issuewright's log.
#[derive(Debug, Default, PartialEq, Eq)]
struct Today {
    claimed: usize,
    to_ci: usize,
    ci_red: usize,
    approved: usize,
    closed: usize,
    to_you: usize,
}

fn count_today<'a>(lines: impl Iterator<Item = &'a str>) -> Today {
    let mut t = Today::default();
    for l in lines {
        match stats::classify(l).map(|(_, k)| k) {
            Some(stats::Kind::Claimed) => t.claimed += 1,
            Some(stats::Kind::ToCi) => t.to_ci += 1,
            Some(stats::Kind::CiRed) => t.ci_red += 1,
            Some(stats::Kind::Approved) => t.approved += 1,
            Some(stats::Kind::Closed) => t.closed += 1,
            Some(stats::Kind::ToYou | stats::Kind::ChangesRequested) => t.to_you += 1,
            _ => {}
        }
    }
    t
}

/// Today's numbers, and a sparkline of the last day's busy slots.
fn today_line(v: &View, cap: usize) -> Line<'static> {
    let t = count_today(v.today());
    let n = |k: usize, what: &str, style: Style| -> Vec<Span<'static>> {
        vec![
            Span::styled(k.to_string(), if k > 0 { style } else { dim() }),
            Span::styled(format!(" {what}  "), dim()),
        ]
    };
    let mut spans = vec![Span::styled("today  ", dim())];
    spans.extend(n(t.claimed, "started", Style::new().cyan()));
    spans.extend(n(t.to_ci, "to CI", Style::new().blue()));
    spans.extend(n(t.ci_red, "CI red", Style::new().yellow()));
    spans.extend(n(t.approved, "approved", Style::new().green()));
    spans.extend(n(t.closed, "closed", Style::new().green()));
    spans.extend(n(t.to_you, "to you", Style::new().yellow()));
    let spark = sparkline(&v.status.usage, cap, Utc::now(), 48);
    if !spark.is_empty() {
        spans.push(Span::styled("   slots, 24h ", dim()));
        spans.push(Span::styled(spark, Style::new().cyan()));
    }
    Line::from(spans)
}

/// The peak of busy slots per half hour over the last day, as block
/// characters scaled to `cap`; `width` characters, oldest first. Empty without
/// samples.
fn sparkline(
    usage: &[crate::state::Usage],
    cap: usize,
    now: DateTime<Utc>,
    width: usize,
) -> String {
    const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if usage.is_empty() {
        return String::new();
    }
    let span = chrono::Duration::days(1).num_seconds();
    let mut peaks = vec![None::<usize>; width];
    for u in usage {
        let age = (now - u.at).num_seconds();
        if !(0..span).contains(&age) {
            continue;
        }
        let i = width - 1 - (age * width as i64 / span) as usize;
        peaks[i] = Some(peaks[i].unwrap_or(0).max(u.busy));
    }
    let cap = cap.max(1);
    peaks
        .into_iter()
        .map(|p| match p {
            None => '·',
            Some(b) => BARS[(b.min(cap) * 8).div_ceil(cap)],
        })
        .collect()
}

fn session_rows(cfg: &Config, v: &View) -> Rows {
    let mut out = Rows::default();
    for r in &cfg.repos {
        for i in 1..=r.slots {
            let slot = format!("{:<10} {i} ", v.repo(r.short()));
            let Some(l) = v
                .locks
                .iter()
                .find(|l| l.repo.eq_ignore_ascii_case(&r.name) && l.slot == i)
            else {
                out.push(
                    Line::from(vec![Span::styled(slot, dim()), Span::styled("free", dim())]),
                    None,
                );
                continue;
            };
            let alive = match (&l.tmux_window, &v.windows) {
                (Some(w), Some(ws)) if !ws.contains(w) => Some("ended"),
                _ => None,
            };
            let done = l.done_since.is_some()
                || v.status
                    .done_waiting_to_close
                    .iter()
                    .any(|d| d.key() == l.issue.key());
            let mut spans = vec![
                Span::raw(slot),
                Span::styled(format!("{:<10}", l.kind.to_string()), kind_style(l)),
                Span::styled(format!("#{:<6}", l.issue.number), Style::new().bold()),
                Span::styled(format!("{:>6} ", ago(l.started_at)), dim()),
            ];
            if let Some(a) = alive {
                spans.push(Span::styled(format!("{a} "), Style::new().yellow()));
            } else if let Some(w) = v.waiting_of(l) {
                spans.push(Span::styled(
                    format!("asks you {} ", ago(w.since)),
                    Style::new().black().on_yellow().bold(),
                ));
                let note = match w.nudged_at {
                    Some(_) => "moving it to GitHub ".to_string(),
                    None if v.question_timeout > 0 => {
                        let left = v.question_timeout as i64 - (Utc::now() - w.since).num_seconds();
                        format!(
                            "GitHub in {} ",
                            ago(Utc::now() - chrono::Duration::seconds(left.max(0)))
                        )
                    }
                    None => String::new(),
                };
                spans.push(Span::styled(note, Style::new().yellow()));
            } else if done {
                spans.push(Span::styled("done ", Style::new().green()));
            }
            spans.push(Span::styled(title_of(v, &l.issue), dim()));
            out.push(
                Line::from(spans),
                Some(Target {
                    issue: l.issue.clone(),
                    window: l.tmux_window.clone(),
                }),
            );
        }
    }
    out
}

fn kind_style(l: &SlotLock) -> Style {
    match l.kind {
        Kind::Implement => Style::new().cyan(),
        Kind::Review => Style::new().magenta(),
        Kind::EpicCheck => Style::new().blue(),
    }
}

fn title_of(v: &View, r: &IssueRef) -> String {
    v.status
        .graph
        .iter()
        .find(|n| n.issue.key() == r.key())
        .map(|n| n.title.clone())
        .unwrap_or_default()
}

fn target(r: &IssueRef) -> Option<Target> {
    Some(Target {
        issue: r.clone(),
        window: None,
    })
}

const STAGES: [(&str, Color); 7] = [
    (NEEDS_INPUT, Color::Yellow),
    (RUNNING, Color::Cyan),
    (CI, Color::Blue),
    (REVIEW, Color::Magenta),
    (CHANGES, Color::Yellow),
    (APPROVED, Color::Green),
    (MERGING, Color::Green),
];

fn pipeline_rows(v: &View) -> Rows {
    let mut out = Rows::default();
    if v.status.graph.is_empty() && !v.status.plan.entries.is_empty() {
        out.push(
            Line::styled("restart issuewright to record the pipeline", dim()),
            None,
        );
        return out;
    }
    for (label, color) in STAGES {
        for n in v
            .status
            .graph
            .iter()
            .filter(|n| n.has(label) && n.state == IssueState::Open)
        {
            let stage = label.trim_start_matches("dispatch:");
            out.push(
                Line::from(vec![
                    Span::styled(format!("{stage:<12}"), Style::new().fg(color)),
                    Span::styled(format!("{:<16} ", v.name(&n.issue)), Style::new().bold()),
                    Span::styled(n.title.clone(), dim()),
                ]),
                target(&n.issue),
            );
        }
    }
    let ready = v.status.plan.entries.len();
    if ready > 0 {
        out.push(
            Line::from(vec![
                Span::styled(format!("{:<12}", "ready"), Style::new().white()),
                Span::styled(format!("{ready} issue(s), see Blockers"), dim()),
            ]),
            None,
        );
    }
    if out.lines.is_empty() {
        out.push(Line::styled("nothing in flight", dim()), None);
    }
    out
}

fn end_style(e: &End) -> (&'static str, Style) {
    let style = match e {
        End::Moving(_) => Style::new().green(),
        End::Queued(_) => Style::new().cyan(),
        End::NeedsInput => Style::new().yellow().bold(),
        End::Parked | End::Skipped(_) => Style::new().yellow(),
        End::Unowned | End::Dead | End::Unresolvable(_) | End::Cycle(_) => {
            Style::new().red().bold()
        }
    };
    (e.mark(), style)
}

fn dependency_rows(v: &View) -> Rows {
    let mut out = Rows::default();
    if v.status.graph.is_empty() && !v.status.plan.entries.is_empty() {
        out.push(
            Line::styled(
                "This issuewright doesn't record blockers yet; restart it after `cargo install --path .`.",
                Style::new().yellow(),
            ),
            None,
        );
    }
    let heading = |text: &'static str, style: Style| {
        Line::styled(
            text,
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
    };
    let mut flowing_header = false;
    if v.stalls.iter().any(|s| !s.end.flows()) {
        out.push(
            heading("Won't move until you act", Style::new().red()),
            None,
        );
    }
    for s in &v.stalls {
        if s.end.flows() && !flowing_header {
            flowing_header = true;
            if !out.lines.is_empty() {
                out.push(Line::raw(""), None);
            }
            out.push(
                heading("Moving: waits clear on their own", Style::new().green()),
                None,
            );
        }
        let (mark, style) = end_style(&s.end);
        let mut what = s.end.describe();
        if let End::Moving(l) = &s.end
            && l == RUNNING
            && let Some(lock) = v.locks.iter().find(|l| l.issue.key() == s.at.key())
        {
            what = format!("{what} in slot {}, {}", lock.slot, ago(lock.started_at));
        }
        out.push(
            Line::from(vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(v.name(&s.at), Style::new().bold()),
                "  ".into(),
                Span::styled(s.title.clone(), dim()),
            ]),
            target(&s.at),
        );
        out.push(
            Line::from(vec![
                "  ".into(),
                Span::styled(what, style),
                Span::styled(format!("  · holds {}", s.held), Style::new().bold()),
            ]),
            None,
        );
        for l in &s.tree {
            let mut spans = vec![
                Span::styled(format!("  {}", l.guide), dim()),
                Span::raw(format!("{} ", v.rel(&l.issue, &s.at))),
            ];
            let with: Vec<IssueRef> = l
                .with
                .iter()
                .filter(|w| !l.also.iter().any(|a| a.key() == w.key()))
                .cloned()
                .collect();
            if !with.is_empty() {
                spans.push(Span::styled(
                    format!("+{} ", v.names(&with, &s.at)),
                    Style::new().fg(Color::Gray),
                ));
            }
            if !l.also.is_empty() {
                spans.push(Span::styled(
                    format!("also held by {} ", v.names(&l.also, &s.at)),
                    Style::new().yellow(),
                ));
            }
            spans.push(Span::styled(l.title.clone(), dim()));
            out.push(Line::from(spans), target(&l.issue));
        }
    }

    let free: Vec<_> = v
        .status
        .plan
        .entries
        .iter()
        .filter(|e| !graph::waits(&v.status.graph, &e.issue) || v.status.graph.is_empty())
        .collect();
    if !free.is_empty() {
        if !out.lines.is_empty() {
            out.push(Line::raw(""), None);
        }
        out.push(heading("Ready, no open blocker", Style::new()), None);
        for e in free {
            let (tag, style, why) = match &e.verdict {
                Verdict::Claim => ("claim", Style::new().green(), String::new()),
                Verdict::Waiting(w) => ("wait ", Style::new().cyan(), w.clone()),
                Verdict::Skipped(s) => ("skip ", Style::new().yellow(), s.clone()),
            };
            let mut spans = vec![
                Span::styled(format!("{tag} "), style),
                Span::styled(v.name(&e.issue), Style::new().bold()),
                "  ".into(),
                Span::styled(e.title.clone(), dim()),
            ];
            if !why.is_empty() {
                spans.push(Span::styled(format!("  {why}"), style));
            }
            out.push(Line::from(spans), target(&e.issue));
        }
    }
    if out.lines.is_empty() {
        out.push(
            Line::styled("No ready issue waits on anything.", dim()),
            None,
        );
    }
    out
}

/// The last log lines, newest at the bottom, colored by what happened.
fn event_lines(v: &View) -> Vec<Line<'static>> {
    let skip = v.log.len().saturating_sub(EVENTS);
    v.log[skip..]
        .iter()
        .map(|(t, l)| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", t.with_timezone(&Local).format("%H:%M")),
                    dim(),
                ),
                Span::styled(v.shorten(l), event_style(l)),
            ])
        })
        .collect()
}

fn event_style(l: &str) -> Style {
    let has = |xs: &[&str]| xs.iter().any(|x| l.contains(x));
    if has(&["poll failed", "couldn't", "claim failed", "error"]) {
        Style::new().red()
    } else if has(&[
        "needs-input",
        "needs input",
        "handed the PR back",
        "CI failed",
        "question",
    ]) {
        Style::new().yellow()
    } else if has(&["approved", "CI is green", "is closed"]) {
        Style::new().green()
    } else if has(&["claimed", "reviewing", "resumed", "checking the epic"]) {
        Style::new().cyan()
    } else {
        Style::new()
    }
}

/// How often the Stats tab rereads the log, and the background thread the
/// transcripts.
const STATS_REFRESH: Duration = Duration::from_secs(30);
const TOKENS_REFRESH: Duration = Duration::from_secs(60);

/// The Stats tab's data. The log is small and read on refresh; the
/// transcripts are large, so a thread started on the first visit reads them,
/// then only what was appended.
#[derive(Default)]
struct StatsTab {
    days: Vec<Day>,
    loaded: Option<Instant>,
    tokens: Option<SharedTokens>,
}

impl StatsTab {
    fn refresh(&mut self, cfg: &Config) {
        let shared = self.tokens.get_or_insert_with(|| {
            let shared = Arc::new(Mutex::new(None));
            let (out, cfg) = (Arc::clone(&shared), cfg.clone());
            std::thread::spawn(move || {
                let mut scan = stats::TokenScan::default();
                loop {
                    scan.update(&cfg);
                    if let Ok(mut o) = out.lock() {
                        *o = Some(scan.by_day.clone());
                    }
                    std::thread::sleep(TOKENS_REFRESH);
                }
            });
            shared
        });
        let tokens = shared.lock().ok().and_then(|t| t.clone());
        let fresh = self.loaded.is_some_and(|t| t.elapsed() < STATS_REFRESH);
        // Reread when tokens first arrive, too.
        let had_tokens = self.days.iter().any(|d| d.tokens.output > 0);
        if fresh && (had_tokens || tokens.is_none()) {
            return;
        }
        let mut days = stats::days(&stats::read_log(cfg), Local::now().date_naive());
        if let Some(t) = &tokens {
            stats::add_tokens(&mut days, t);
        }
        self.days = days;
        self.loaded = Some(Instant::now());
    }

    fn counting_tokens(&self) -> bool {
        self.tokens
            .as_ref()
            .is_some_and(|t| t.lock().map(|t| t.is_none()).unwrap_or(false))
    }
}

/// Days the chart shows: as many as fit, at most the last 30.
fn chart_days(width: u16) -> usize {
    // Three bars of 3, a gap of 1 between them, 2 between days.
    ((width.saturating_sub(2)) / 13).clamp(1, 30) as usize
}

fn draw_stats(f: &mut Frame, s: &StatsTab, app: &App) {
    let [head, chart, table, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(12),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let week = &s.days[s.days.len().saturating_sub(7)..];
    let mut head_lines = vec![Line::from(vec![
        Span::styled("issuewright stats", Style::new().bold()),
        Span::styled("  last 7 days  ", dim()),
        Span::styled(
            if s.counting_tokens() {
                "counting tokens in the transcripts…"
            } else {
                ""
            },
            Style::new().yellow(),
        ),
    ])];
    if week.is_empty() {
        head_lines.push(Line::styled("No events in issuewright's log yet.", dim()));
    } else {
        head_lines.push(Line::raw(stats::summary(week)));
    }
    f.render_widget(Paragraph::new(head_lines).wrap(Wrap { trim: true }), head);

    let n = chart_days(chart.width);
    let shown = &s.days[s.days.len().saturating_sub(n)..];
    let mut bars = BarChart::default()
        .block(
            Block::bordered()
                .title(Line::from(vec![
                    " per day: ".into(),
                    Span::styled("started ", Style::new().cyan()),
                    Span::styled("approved ", Style::new().green()),
                    Span::styled("CI red ", Style::new().yellow()),
                ]))
                .border_style(dim()),
        )
        .bar_width(3)
        .bar_gap(1)
        .group_gap(2);
    for d in shown {
        let bar = |v: usize, c: Color| {
            Bar::new(v as u64)
                .style(Style::new().fg(c))
                .value_style(Style::new().fg(Color::Black).bg(c))
        };
        bars = bars.data(
            BarGroup::new(vec![
                bar(d.claimed, Color::Cyan),
                bar(d.approved, Color::Green),
                bar(d.ci_red, Color::Yellow),
            ])
            .label(Line::from(d.date.format("%m-%d").to_string()).centered()),
        );
    }
    f.render_widget(bars, chart);

    let (header, rows) = stats::table(&s.days);
    let widths: Vec<Constraint> = header
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let w = rows
                .iter()
                .map(|r| r[i].chars().count())
                .chain([h.chars().count()])
                .max()
                .unwrap_or(1);
            Constraint::Length(w as u16)
        })
        .collect();
    let rows: Vec<Row> = rows.into_iter().map(Row::new).collect();
    f.render_widget(
        Table::new(rows, widths)
            .header(Row::new(header).style(Style::new().bold()))
            .column_spacing(2)
            .block(
                Block::bordered()
                    .title(" per day, newest first ")
                    .border_style(dim()),
            ),
        table,
    );
    let line = match &app.message {
        Some((m, at)) if at.elapsed() < Duration::from_secs(6) => {
            Line::styled(m.clone(), Style::new().yellow())
        }
        _ => Line::styled(
            "q quit · 1-3 or Esc back to the live panes · r reload · lead: first claim to approval · CI: to CI until green or red, queue included",
            dim(),
        ),
    };
    f.render_widget(Paragraph::new(line), footer);
}

fn footer_line(f: &mut Frame, area: Rect, app: &App) {
    let line = match &app.message {
        Some((m, at)) if at.elapsed() < Duration::from_secs(6) => {
            Line::styled(m.clone(), Style::new().yellow())
        }
        _ => Line::styled(
            "q quit · Tab/1-3 pane · 4 stats · ↑↓ jk PgUp PgDn g G select · Enter go to session · o open on GitHub · r reload",
            dim(),
        ),
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_todays_events() {
        let log = "\
2026-11-22T08:00:00Z mono#1: claimed, slot 1, window @1, branch feat/1-x
2026-11-22T08:30:00Z mono#1: finished (its issue is dispatch:ci), the session exits
2026-11-22T08:40:00Z mono#1: CI failed on o/mono#2 at abc1234, resumed in slot 1, window @2
2026-11-22T09:00:00Z mono#1-review: finished (it handed the PR back), the session exits
2026-11-22T09:10:00Z mono#3: approved, auto-merge enabled on o/mono#4
hint: a continuation line
2026-11-22T09:20:00Z mono#3: its issue is closed, closed its window
";
        let parsed = parse_log(log);
        assert_eq!(parsed.len(), 6);
        let t = count_today(parsed.iter().map(|(_, l)| l.as_str()));
        assert_eq!(
            t,
            Today {
                claimed: 1,
                to_ci: 1,
                ci_red: 1,
                approved: 1,
                closed: 1,
                to_you: 1
            }
        );
    }

    #[test]
    fn sparkline_shows_half_hour_peaks_scaled_to_the_slots() {
        let now: DateTime<Utc> = "2026-11-22T12:00:00Z".parse().unwrap();
        let u = |mins: i64, busy| crate::state::Usage {
            at: now - chrono::Duration::minutes(mins),
            busy,
        };
        let s = sparkline(&[u(70, 1), u(40, 2), u(10, 4), u(5, 3)], 4, now, 48);
        assert_eq!(s.chars().count(), 48);
        assert!(s.ends_with("▄█"), "{s}");
        assert!(s.starts_with('·'));
        assert_eq!(sparkline(&[], 4, now, 48), "");
    }

    #[test]
    fn selection_steps_over_lines_without_a_target() {
        let r = |n| target(&IssueRef::new("o/a", n));
        let rows = Rows {
            lines: vec![Line::raw(""); 5],
            targets: vec![None, r(1), None, r(2), r(3)],
        };
        assert_eq!(rows.first(), Some(1));
        assert_eq!(rows.step(1, 1), Some(3));
        assert_eq!(rows.step(3, -1), Some(1));
        assert_eq!(rows.step(1, 99), Some(4));
        assert_eq!(rows.step(4, -99), Some(1));
    }

    #[test]
    fn common_prefix_ends_at_a_dash() {
        let p = |xs: &[&str]| common_prefix(xs.iter().copied());
        assert_eq!(p(&["acme-api", "acme-web"]), "acme-");
        assert_eq!(p(&["app-one", "apple"]), "");
        assert_eq!(p(&["acme-api"]), "");
        assert_eq!(p(&[]), "");
    }
}
