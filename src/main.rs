//! issuewright: polls GitHub for issues labeled `dispatch:ready` and starts one
//! fresh Claude Code session per issue in a warm worktree slot.

mod app;
mod body;
mod config;
mod daemon;
mod github;
mod graph;
mod health;
mod hook;
mod model;
mod scheduler;
mod session;
mod setup_app;
mod signing;
mod slots;
mod state;
mod stats;
mod tmux;
mod tools;
mod watch;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{Config, default_config_path};
use crate::model::IssueRef;
use crate::scheduler::Verdict;
use crate::state::{Kind, Mode, State};

#[derive(Parser)]
#[command(
    version,
    about = "Starts one fresh Claude Code session per GitHub issue labeled dispatch:ready"
)]
struct Cli {
    /// Config file (default: ~/.config/issuewright/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Poll and start sessions until stopped.
    Run {
        #[arg(long, value_enum, default_value_t = Mode::Interactive)]
        mode: Mode,
        /// Print the scheduled order with reasons; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// One poll cycle, then wait for the sessions it started.
        #[arg(long)]
        once: bool,
    },
    /// Slots, sessions, the queue with wait reasons, and the last poll.
    Status {
        /// Show the queue as trees of blockers: each issue that holds ready
        /// issues back, with everything waiting on it underneath.
        #[arg(long)]
        tree: bool,
    },
    /// Throughput and efficiency per day, from issuewright's log and the slots'
    /// transcripts.
    Stats {
        /// How many days, ending today.
        #[arg(long, default_value_t = 14)]
        days: usize,
        /// Skip reading the transcripts for token counts (faster).
        #[arg(long)]
        no_tokens: bool,
    },
    /// A live screen of sessions, the pipeline, and which issue holds which
    /// others back. Reads issuewright's state; changes nothing.
    Watch,
    /// Print the settings and environment a session in this repo's slot gets.
    ShowSession {
        /// `owner/name` of a configured repository.
        repo: String,
        #[arg(long, default_value_t = 1)]
        slot: usize,
        /// Write the settings to this file instead of printing them.
        #[arg(long)]
        write_settings: Option<PathBuf>,
        /// Show a reviewer session (`auto_merge`) instead.
        #[arg(long)]
        review: bool,
    },
    /// Create the review app on GitHub with a prefilled manifest, save its
    /// key next to the config and add `[review_app]` to it.
    SetupApp {
        /// Create it in this organization instead of your personal account.
        #[arg(long)]
        org: Option<String>,
        /// The app's name (default: `<org or login>-issuewright`).
        #[arg(long)]
        name: Option<String>,
        /// Local port for GitHub's redirect (default: any free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Commands a session runs outside its sandbox, on its own slot only.
    /// issuewright tells its sessions about them; they need the session's key.
    Session {
        #[command(subcommand)]
        tool: tools::Tool,
    },
    /// A session's `Stop` hook: exits the session in this slot once its issue
    /// is at the end of its part. issuewright sets it up; not for direct use.
    #[command(hide = true)]
    StopHook {
        /// `owner/name` of the session's repository.
        #[arg(long)]
        repo: String,
        #[arg(long)]
        slot: usize,
    },
    /// A session's `UserPromptSubmit` hook: the session got an answer.
    /// issuewright sets it up; not for direct use.
    #[command(hide = true)]
    PromptHook {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        slot: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Run by a session: trusts neither the config path nor the environment
    // it was given (see the tools module).
    let command = match cli.command {
        Cmd::Session { tool } => return tools::run(tool),
        c => c,
    };
    let config_path = cli.config.unwrap_or_else(default_config_path);
    // Works before there is a config.
    if let Cmd::SetupApp { org, name, port } = command {
        return setup_app::run(&config_path, setup_app::Options { org, name, port });
    }
    let cfg = Config::load(&config_path)?;
    match command {
        Cmd::Run {
            mode,
            dry_run,
            once,
        } => {
            let stop = Arc::new(AtomicBool::new(false));
            for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
                signal_hook::flag::register(sig, Arc::clone(&stop))?;
            }
            daemon::Daemon::new(
                cfg,
                daemon::Options {
                    config: std::path::absolute(&config_path)?,
                    mode,
                    dry_run,
                    once,
                },
                stop,
            )?
            .run()
        }
        Cmd::Status { tree } => status(&cfg, tree),
        Cmd::Stats { days, no_tokens } => stats::print(&cfg, days, !no_tokens),
        Cmd::Watch => watch::run(&cfg),
        Cmd::Session { .. } | Cmd::SetupApp { .. } => unreachable!("handled above"),
        Cmd::StopHook { repo, slot } => hook::run(&cfg, &repo, slot),
        Cmd::PromptHook { repo, slot } => hook::answered(&cfg, &repo, slot),
        Cmd::ShowSession {
            repo,
            slot,
            write_settings,
            review,
        } => {
            let kind = if review {
                Kind::Review
            } else {
                Kind::Implement
            };
            show_session(&cfg, &config_path, &repo, slot, write_settings, kind)
        }
    }
}

/// The queue as trees of blockers, those that need you first, then the
/// ready issues nothing holds back.
fn print_trees(st: &state::Status, stalls: &[graph::Stall]) {
    let mut moving_header = false;
    if stalls.iter().any(|s| !s.end.flows()) {
        println!("Won't move until you act:");
    }
    for s in stalls {
        if s.end.flows() && !moving_header {
            moving_header = true;
            println!("\nMoving: these waits clear on their own:");
        }
        println!(
            "{} {:<24} {}",
            s.end.mark(),
            s.at.short(),
            truncate(&s.title, 60)
        );
        println!("    {} · holds {}", s.end.describe(), s.held);
        for l in &s.tree {
            let mut extra = String::new();
            let with: Vec<String> = l
                .with
                .iter()
                .filter(|w| !l.also.iter().any(|a| a.key() == w.key()))
                .map(IssueRef::short)
                .collect();
            if !with.is_empty() {
                extra.push_str(&format!(" +{}", with.join(", ")));
            }
            if !l.also.is_empty() {
                let also: Vec<String> = l.also.iter().map(IssueRef::short).collect();
                extra.push_str(&format!(" (also held by {})", also.join(", ")));
            }
            println!(
                "    {}{}{extra}  {}",
                l.guide,
                l.issue.short(),
                truncate(&l.title, 50)
            );
        }
    }
    let free: Vec<_> = st
        .plan
        .entries
        .iter()
        .filter(|e| !matches!(e.verdict, Verdict::Skipped(_)) && !graph::waits(&st.graph, &e.issue))
        .collect();
    if !free.is_empty() {
        if !stalls.is_empty() {
            println!();
        }
        println!("Ready, no open blocker (scheduled order):");
        for e in free {
            let (tag, why) = match &e.verdict {
                Verdict::Claim => ("CLAIM", String::new()),
                Verdict::Waiting(w) => ("wait ", format!("  {w}")),
                Verdict::Skipped(_) => unreachable!(),
            };
            let unlocks = match e.unlocks {
                0 => String::new(),
                n => format!(" · unlocks {n}"),
            };
            println!(
                "  {tag} {:<24} {}{unlocks}{why}",
                e.issue.short(),
                truncate(&e.title, 50)
            );
        }
    } else if stalls.is_empty() {
        println!("No {} issues.", model::READY);
    }
    for n in &st.plan.notes {
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

fn show_session(
    cfg: &Config,
    config_path: &std::path::Path,
    repo: &str,
    slot: usize,
    write: Option<PathBuf>,
    kind: Kind,
) -> Result<()> {
    let r = cfg
        .repo(repo)
        .with_context(|| format!("{repo} is not configured"))?;
    let s = slots::Slot::new(cfg, r, slot);
    let name = format!("{}#<number>", r.short());
    let notify = cfg
        .notify_command
        .replace("{repo}", &r.name)
        .replace("{number}", "<number>")
        .replace("{name}", &name);
    let exe = hook::current_exe()?;
    let config = std::path::absolute(config_path)?;
    let stop = hook::command(&exe, &config, "stop-hook", &r.name, slot);
    let prompt = hook::command(&exe, &config, "prompt-hook", &r.name, slot);
    let hooks = session::Hooks {
        notify: &notify,
        stop: &stop,
        prompt: &prompt,
        issuewright: &exe.to_string_lossy(),
    };
    let settings = serde_json::to_string_pretty(&session::settings(cfg, r, &s, &hooks, kind)?)?;
    match write {
        Some(p) => std::fs::write(&p, settings)?,
        None => println!("{settings}"),
    }
    eprintln!("\nEnvironment (nothing else is inherited):");
    let public_key = match &cfg.session.signing_key {
        Some(k) if r.signs(true) => Some(signing::public_key(k)?),
        _ => None,
    };
    let socket = signing::socket_path(cfg);
    let signs = public_key.as_deref().map(|k| (socket.as_path(), k));
    for (k, v) in session::environment(cfg, r, &s, "<token>", signs) {
        eprintln!("  {k}={v}");
    }
    eprintln!("\nWorking directory: {}", s.dir.display());
    Ok(())
}

fn status(cfg: &Config, tree: bool) -> Result<()> {
    let state = State::open(cfg.state_dir())?;
    let st = state.status()?;
    let locks = state.locks()?;
    match st.last_poll {
        Some(t) => println!(
            "Last poll: {} ({})",
            t.format("%Y-%m-%d %H:%M:%S UTC"),
            st.mode.map(|m| m.to_string()).unwrap_or_default()
        ),
        None => println!("Last poll: never"),
    }
    if let Some(e) = &st.last_error {
        println!("Last error: {e}");
    }
    if let Some(h) = &st.hold {
        println!("Hold: {h}");
    }
    for a in &st.alarms {
        println!("Alarm: {}", a.summary(chrono::Utc::now()));
    }
    println!("\nSlots:");
    let windows = tmux::window_ids();
    for r in &cfg.repos {
        for i in 1..=r.slots {
            match locks
                .iter()
                .find(|l| l.repo.eq_ignore_ascii_case(&r.name) && l.slot == i)
            {
                None => println!("  {} {i}: free", r.short()),
                Some(l) => {
                    let alive = match (&l.tmux_window, &windows) {
                        (Some(w), Some(ws)) => {
                            if ws.contains(w) {
                                "running"
                            } else {
                                "ended (freed at next poll)"
                            }
                        }
                        _ => "unknown",
                    };
                    let done = match (l.done_since, cfg.close_done_after_secs) {
                        (Some(t), Some(g)) => format!(
                            ", done, closes at {}",
                            (t + chrono::Duration::seconds(g as i64)).format("%H:%M UTC")
                        ),
                        _ if st
                            .done_waiting_to_close
                            .iter()
                            .any(|d| d.key() == l.issue.key()) =>
                        {
                            ", done, waiting to be closed".to_string()
                        }
                        _ => String::new(),
                    };
                    let what = match &l.review {
                        Some(rv) => format!("reviewing {} at {:.7}", rv.pr, rv.head_sha),
                        None => format!("on {}", l.branch),
                    };
                    println!("  {} {i}: {} {what} ({alive}{done})", r.short(), l.name);
                }
            }
        }
    }
    println!();
    let stalls = graph::analyze(&st.graph, &st.plan);
    if tree {
        print_trees(&st, &stalls);
        return Ok(());
    }
    daemon::print_plan(&st.plan, &[]);
    let stuck: Vec<_> = stalls.into_iter().filter(|s| !s.end.flows()).collect();
    if !stuck.is_empty() {
        println!("\nStuck until you act (issuewright status --tree shows the full trees):");
        for s in stuck {
            println!(
                "  {:<24} holds {}: {}",
                s.at.short(),
                s.held,
                s.end.describe()
            );
        }
    }
    Ok(())
}
