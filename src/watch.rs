//! `issuewright watch`: a screen to keep open beside the daemon. It shows the
//! running sessions, the work in flight, and everything that needs you: the
//! questions, the reviews that are yours, and the ends of chains of blockers
//! that won't move until you act, those that free the most work first.
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

    /// The newest log line about `r` that `want` accepts, with its time.
    fn last_event_of(
        &self,
        r: &IssueRef,
        want: impl Fn(stats::Kind) -> bool,
    ) -> Option<DateTime<Utc>> {
        let key = format!(
            "{}#{}",
            r.repo.rsplit('/').next().unwrap_or(&r.repo),
            r.number
        );
        self.log.iter().rev().find_map(|(t, l)| {
            let (k, kind) = stats::classify(l)?;
            (k.eq_ignore_ascii_case(&key) && want(kind)).then_some(*t)
        })
    }

    /// When the log last said something about `r`.
    fn last_event(&self, r: &IssueRef) -> Option<DateTime<Utc>> {
        self.last_event_of(r, |_| true)
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

/// Widest a repository's or issue's name gets in a column; longer ones are
/// cut short in the repository part, keeping the number.
const NAME_MAX: usize = 22;

/// `name` cut to `max` characters with `…`, keeping a `#number` suffix.
fn clip(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let (repo, num) = match name.rfind('#') {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    let keep = max.saturating_sub(num.chars().count() + 1);
    format!("{}…{num}", repo.chars().take(keep).collect::<String>())
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

const PANES: [&str; 3] = ["Sessions", "Pipeline", "Needs you"];

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
            pipeline_rows(cfg, &view),
            needs_you_rows(cfg, &view),
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
    let asks = panes[2].targets.iter().filter(|t| t.is_some()).count();
    let needs_title = if asks > 0 {
        vec![
            " 3 Needs you ".into(),
            Span::styled(format!(" {asks} "), Style::new().black().on_yellow().bold()),
            " ".into(),
        ]
    } else {
        vec![" 3 Needs you ".into()]
    };
    let counts = pipeline_counts(v);
    let titles = [
        vec![format!(" 1 Sessions  {}/{slots} ", v.locks.len()).into()],
        vec![
            " 2 Pipeline ".into(),
            Span::styled(
                if counts.is_empty() {
                    String::new()
                } else {
                    format!(" {counts} ")
                },
                dim(),
            ),
        ],
        needs_title,
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
    spans.extend(tabs(false));
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
    let asking = v.waiting.len();
    spans.push(format!("  ·  {}/{slots} slots", v.locks.len()).into());
    if asking > 0 {
        spans.push("  ·  ".into());
        spans.push(Span::styled(
            format!("{asking} asking you"),
            Style::new().yellow().bold(),
        ));
    }
    let mut out = vec![
        Line::from(spans),
        backlog_line(v),
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

/// The ready work by what it waits on: how much gets done without you.
fn backlog_line(v: &View) -> Line<'static> {
    let b = graph::backlog(&v.status.graph, &v.status.plan);
    let stuck = v.stalls.iter().filter(|s| !s.end.flows()).count();
    let n = |k: usize, what: String, style: Style| -> Vec<Span<'static>> {
        vec![
            Span::styled(k.to_string(), if k > 0 { style } else { dim() }),
            Span::styled(format!(" {what}  "), dim()),
        ]
    };
    let mut spans = vec![Span::styled("ready  ", dim())];
    spans.extend(n(
        b.qualified,
        "qualified".into(),
        Style::new().green().bold(),
    ));
    spans.extend(n(
        b.behind_moving,
        "behind work in flight".into(),
        Style::new().green(),
    ));
    spans.push(Span::styled("= ", dim()));
    spans.extend(n(
        b.unattended(),
        "without you".into(),
        Style::new().green().bold(),
    ));
    spans.push(Span::styled("·  ", dim()));
    let held = if stuck > 0 {
        format!("held by {stuck} stuck")
    } else {
        "held".into()
    };
    spans.extend(n(b.held, held, Style::new().red().bold()));
    spans.extend(n(b.needs_input, "need you".into(), Style::new().yellow()));
    spans.extend(n(b.parked, "parked".into(), Style::new().yellow()));
    spans.extend(n(b.in_flight, "in flight".into(), Style::new().cyan()));
    Line::from(spans)
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

/// The busy slots, those asking you first, then the oldest; the free ones
/// in one line at the end, so the active sessions always fit.
fn session_rows(cfg: &Config, v: &View) -> Rows {
    let mut out = Rows::default();
    let mut busy: Vec<&SlotLock> = v.locks.iter().collect();
    busy.sort_by_key(|l| (v.waiting_of(l).is_none(), l.started_at));
    let w = busy
        .iter()
        .map(|l| v.repo(&l.repo).chars().count())
        .max()
        .unwrap_or(0);
    for l in busy {
        let slot = format!("{:<w$} {} ", v.repo(&l.repo), l.slot);
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
    let free: Vec<String> = cfg
        .repos
        .iter()
        .flat_map(|r| {
            let used = v
                .locks
                .iter()
                .filter(|l| l.repo.eq_ignore_ascii_case(&r.name))
                .count();
            let n = r.slots.saturating_sub(used);
            let name = v.repo(r.short()).to_string();
            (n > 0).then(|| if n > 1 { format!("{name} ×{n}") } else { name })
        })
        .collect();
    if !free.is_empty() {
        let n: usize = cfg.repos.iter().map(|r| r.slots).sum::<usize>() - v.locks.len();
        out.push(
            Line::from(vec![
                Span::styled(
                    format!("{n} free  "),
                    Style::new().bold().fg(Color::DarkGray),
                ),
                Span::styled(free.join(", "), dim()),
            ]),
            None,
        );
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

/// Work in flight, closest to merged first.
const STAGES: [(&str, Color); 6] = [
    (MERGING, Color::Green),
    (APPROVED, Color::Green),
    (CHANGES, Color::Yellow),
    (REVIEW, Color::Magenta),
    (CI, Color::Blue),
    (RUNNING, Color::Cyan),
];

/// The repositories whose PRs a person reviews: no `auto_merge`.
fn reviewed_by_you<'a>(cfg: &'a Config) -> impl Fn(&IssueRef) -> bool + 'a {
    move |r: &IssueRef| {
        cfg.repos
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&r.repo) && !c.auto_merge)
    }
}

fn pipeline_rows(cfg: &Config, v: &View) -> Rows {
    let mut out = Rows::default();
    if v.status.graph.is_empty() && !v.status.plan.entries.is_empty() {
        out.push(
            Line::styled("restart issuewright to record the pipeline", dim()),
            None,
        );
        return out;
    }
    let by_you = reviewed_by_you(cfg);
    let w = v
        .status
        .graph
        .iter()
        .filter(|n| STAGES.iter().any(|(l, _)| n.has(l)))
        .map(|n| v.name(&n.issue).chars().count().min(NAME_MAX))
        .max()
        .unwrap_or(0);
    for (label, color) in STAGES {
        let mut here: Vec<&graph::Node> = v
            .status
            .graph
            .iter()
            .filter(|n| n.has(label) && n.state == IssueState::Open && !n.has(NEEDS_INPUT))
            .collect();
        // Oldest first: what sat longest is at the top of its stage.
        here.sort_by_key(|n| v.last_event(&n.issue));
        for n in here {
            let mut stage = label.trim_start_matches("dispatch:").to_string();
            if label == REVIEW && by_you(&n.issue) {
                stage.push_str(" (you)");
            }
            let since = v
                .last_event(&n.issue)
                .map(ago)
                .unwrap_or_else(|| "–".into());
            out.push(
                Line::from(vec![
                    Span::styled(format!("{stage:<13}"), Style::new().fg(color)),
                    Span::styled(
                        format!("{:<w$} ", clip(&v.name(&n.issue), NAME_MAX)),
                        Style::new().bold(),
                    ),
                    Span::styled(format!("{since:>5}  "), dim()),
                    Span::styled(n.title.clone(), dim()),
                ]),
                target(&n.issue),
            );
        }
    }
    if out.lines.is_empty() {
        out.push(Line::styled("nothing in flight", dim()), None);
    }
    out
}

/// How many of each stage are in flight, for the pane's title.
fn pipeline_counts(v: &View) -> String {
    STAGES
        .iter()
        .filter_map(|(label, _)| {
            let n = v
                .status
                .graph
                .iter()
                .filter(|n| n.has(label) && n.state == IssueState::Open && !n.has(NEEDS_INPUT))
                .count();
            (n > 0).then(|| format!("{n} {}", label.trim_start_matches("dispatch:")))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Something only you can move, with what to do about it.
struct Ask {
    tag: &'static str,
    style: Style,
    issue: IssueRef,
    /// Ready issues that wait on it, directly or through others.
    frees: usize,
    since: Option<DateTime<Utc>>,
    what: String,
    window: Option<String>,
}

/// Everything waiting on you, most work freed first, then the oldest: a
/// session asking you, a question on GitHub, a PR to review, a blocker that
/// won't move on its own.
fn asks(cfg: &Config, v: &View) -> Vec<Ask> {
    let mut out: Vec<Ask> = Vec::new();
    let frees = |r: &IssueRef| {
        v.stalls
            .iter()
            .find(|s| s.at.key() == r.key())
            .map_or(0, |s| s.held)
    };
    for l in &v.locks {
        if let Some(w) = v.waiting_of(l) {
            out.push(Ask {
                tag: "reply",
                style: Style::new().black().on_yellow().bold(),
                issue: l.issue.clone(),
                frees: frees(&l.issue),
                since: Some(w.since),
                what: "the session asks you; Enter goes to it".into(),
                window: l.tmux_window.clone(),
            });
        }
    }
    let asking: BTreeSet<_> = out.iter().map(|a| a.issue.key()).collect();
    let by_you = reviewed_by_you(cfg);
    for n in v
        .status
        .graph
        .iter()
        .filter(|n| n.state == IssueState::Open)
    {
        if asking.contains(&n.issue.key()) {
            continue;
        }
        let since = || v.last_event(&n.issue);
        if n.has(NEEDS_INPUT) {
            let since = v
                .last_event_of(&n.issue, |k| k == stats::Kind::ToYou)
                .or_else(since);
            out.push(Ask {
                tag: "answer",
                style: Style::new().yellow().bold(),
                issue: n.issue.clone(),
                frees: frees(&n.issue),
                since,
                what: String::new(),
                window: None,
            });
        } else if n.has(REVIEW) && by_you(&n.issue) {
            out.push(Ask {
                tag: "review",
                style: Style::new().magenta().bold(),
                issue: n.issue.clone(),
                frees: frees(&n.issue),
                since: since(),
                what: "CI is green: review and merge its PR".into(),
                window: None,
            });
        }
    }
    for s in v.stalls.iter().filter(|s| !s.end.flows()) {
        if matches!(s.end, End::NeedsInput) {
            continue;
        }
        let held: Vec<IssueRef> = s
            .tree
            .iter()
            .filter(|l| !l.guide.starts_with(' ') && !l.guide.starts_with('│'))
            .map(|l| l.issue.clone())
            .collect();
        let held = v.names(&held, &s.at);
        let what = match &s.end {
            End::Unowned => {
                "open without a dispatch label: label it dispatch:ready, or close it".into()
            }
            End::Dead => format!("closed as not planned: drop it from the blockers of {held}"),
            End::Parked => "status:blocked: remove the label once it can go".into(),
            End::Skipped(r) if r.starts_with("epic") => {
                format!("an epic as blocker: point {held} at its sub-issues, or close it once done")
            }
            End::Skipped(r) => format!("skipped ({r})"),
            End::Unresolvable(w) => {
                format!("can't be looked up ({w}): fix the reference in {held}")
            }
            End::Cycle(m) => format!(
                "deadlock: {} wait on each other; drop one dependency",
                v.names(m, &s.at)
            ),
            End::NeedsInput | End::Moving(_) | End::Queued(_) => continue,
        };
        out.push(Ask {
            tag: "unblock",
            style: Style::new().red().bold(),
            issue: s.at.clone(),
            frees: s.held,
            since: None,
            what,
            window: None,
        });
    }
    out.sort_by(|a, b| {
        b.frees
            .cmp(&a.frees)
            .then(a.since.is_none().cmp(&b.since.is_none()))
            .then(a.since.cmp(&b.since))
            .then(a.issue.key().cmp(&b.issue.key()))
    });
    out
}

fn needs_you_rows(cfg: &Config, v: &View) -> Rows {
    let mut out = Rows::default();
    let asks = asks(cfg, v);
    let w = asks
        .iter()
        .map(|a| v.name(&a.issue).chars().count().min(NAME_MAX))
        .max()
        .unwrap_or(0);
    for a in asks {
        let frees = match a.frees {
            0 => String::new(),
            n => format!("frees {n}"),
        };
        let since = a.since.map(ago).unwrap_or_else(|| "–".into());
        let mut spans = vec![
            Span::styled(format!("{:<8}", a.tag), a.style),
            Span::styled(
                format!("{:<w$} ", clip(&v.name(&a.issue), NAME_MAX)),
                Style::new().bold(),
            ),
            Span::styled(format!("{frees:<9}"), Style::new().green().bold()),
            Span::styled(format!("{since:>5}  "), dim()),
        ];
        if !a.what.is_empty() {
            spans.push(Span::styled(format!("{}  ", a.what), Style::new().white()));
        }
        spans.push(Span::styled(title_of(v, &a.issue), dim()));
        out.push(
            Line::from(spans),
            Some(Target {
                issue: a.issue,
                window: a.window,
            }),
        );
    }
    if out.lines.is_empty() {
        out.push(
            Line::styled(
                "Nothing waits on you. Blocker trees: issuewright status --tree",
                dim(),
            ),
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
        if let Ok(h) = crate::state::State::open(cfg.state_dir()).and_then(|s| s.backlog()) {
            stats::add_backlog(&mut days, &h);
        }
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

fn stats_table(
    header: Vec<&'static str>,
    rows: Vec<Vec<String>>,
    title: &'static str,
) -> Table<'static> {
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
    let right = |i: usize, c: String| {
        if i == 0 {
            Line::from(c)
        } else {
            Line::from(c).right_aligned()
        }
    };
    let row =
        |cells: Vec<String>| Row::new(cells.into_iter().enumerate().map(|(i, c)| right(i, c)));
    Table::new(rows.into_iter().map(row).collect::<Vec<_>>(), widths)
        .header(row(header.iter().map(|h| h.to_string()).collect()).style(Style::new().bold()))
        .column_spacing(2)
        .block(Block::bordered().title(title).border_style(dim()))
}

/// The tab bar: the live panes or the Stats tab.
fn tabs(stats: bool) -> Vec<Span<'static>> {
    let tab = |name: &'static str, on: bool| {
        if on {
            Span::styled(name, Style::new().black().on_cyan().bold())
        } else {
            Span::styled(name, dim())
        }
    };
    vec![
        tab(" Live 1-3 ", !stats),
        " ".into(),
        tab(" Stats 4 ", stats),
        "  ".into(),
    ]
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
    let mut head_spans = vec![
        Span::styled("issuewright", Style::new().bold()),
        "  ".into(),
    ];
    head_spans.extend(tabs(true));
    let mut head_lines = vec![Line::from(
        [
            head_spans,
            vec![
                Span::styled("last 7 days  ", dim()),
                Span::styled(
                    if s.counting_tokens() {
                        "counting tokens in the transcripts…"
                    } else {
                        ""
                    },
                    Style::new().yellow(),
                ),
            ],
        ]
        .concat(),
    )];
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

    let [work, flow] = stats::tables(&s.days);
    let [work_area, flow_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(table);
    for ((header, rows), area, title) in [
        (work, work_area, " what got done, per day "),
        (
            flow,
            flow_area,
            " how long, what it cost, and the backlog at the day's last poll ",
        ),
    ] {
        f.render_widget(stats_table(header, rows, title), area);
    }
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
            "q quit · Tab/1-3 pane · 4 Stats tab · ↑↓ jk PgUp PgDn g G select · Enter go to session · o open on GitHub · r reload",
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
    fn clip_keeps_the_issue_number() {
        assert_eq!(clip("mono#12", 22), "mono#12");
        assert_eq!(
            clip("terraform-provider-origincloud#51", 22),
            "terraform-provider…#51"
        );
        assert_eq!(
            clip("terraform-provider-origincloud#51", 22)
                .chars()
                .count(),
            22
        );
        assert_eq!(
            clip("terraform-provider-origincloud", 16),
            "terraform-provi…"
        );
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
