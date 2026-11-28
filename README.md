# issuewright

issuewright watches GitHub repositories for issues labeled `dispatch:ready`. For each one it starts a fresh Claude Code session in a warm worktree, in the order the issues' dependencies and epics allow.

[docs/flow.md](docs/flow.md) shows the whole process and its decisions as diagrams.

One session per issue keeps the context small. Every request in a long-lived session resends everything that session has seen, so work on many issues in one session gets expensive. With issuewright, the issue tracker is the only state shared between sessions.

## What this is, and isn't

issuewright is the tool I run my own repositories with, shared as is. It fits one setup: Linux, tmux, GitHub, the `gh` CLI and Claude Code. Its labels, prompts and merge rules follow how I work.

- It isn't a product, and there is no support. Issues and pull requests are welcome, but I may not get to them, and I may decline changes that make it more general than I need.
- It depends on Claude Code's hooks, settings and transcript layout. A Claude Code release can break it before issuewright catches up.
- Sessions run in Claude Code's sandbox with the permissions in [Guardrails](#guardrails). The sandbox limits what a session can write and reach; it doesn't make a session's work trustworthy. A session can still push wrong code to its branch, and with `auto_merge` an approved PR merges without a human. `auto_merge` is off unless you turn it on per repository; read [Review and merge](#review-and-merge) first.
- You're responsible for using Claude Code within the terms of your plan.

## Requirements

- Linux, with `git`, `gh` (logged in), `tmux` and `claude` (logged in to claude.ai, for Remote Control) on `PATH`
- `bubblewrap` and `socat` for Claude Code's sandbox: `sudo apt-get install bubblewrap socat`
- To install: `cargo install issuewright` (Rust 1.89 or newer), or a static binary for x86_64 or aarch64 from the [releases](https://github.com/mrcrgl/issuewright/releases)

Tested with Claude Code 2.1.325, gh 2.x and tmux 3.5a.

## Quick start

```sh
mkdir -p ~/.config/issuewright
cp config.example.toml ~/.config/issuewright/config.toml   # list your repositories
issuewright run --dry-run                                  # the queue, with reasons; changes nothing
issuewright run                                            # interactive mode, in tmux session "agents"
issuewright status                                         # slots, sessions, queue, last poll
issuewright status --tree                                  # the queue as trees of blockers
issuewright watch                                          # live screen: sessions, pipeline, what needs you, events
```

`issuewright run` creates the `dispatch:*` labels in every configured repository. To release an issue, label it `dispatch:ready`.

## Labels

| Label | Set by | Meaning |
|---|---|---|
| `dispatch:ready` | you | Released for a session |
| `dispatch:running` | issuewright, when it claims the issue or starts an epic check | A session is working on it |
| `dispatch:needs-input` | the session, or issuewright when a session ends without an end label | Waiting for you; the latest comment says why |
| `dispatch:ci` | the session | PR pushed; the session exited and issuewright waits for the PR's checks |
| `dispatch:review` | issuewright, once CI is green | PR open with green checks, ready for review |
| `dispatch:changes-requested` | the reviewer session (`auto_merge`) | The reviewer requested changes the PR's author can make; issuewright resumes the session that wrote the PR |
| `dispatch:approved` | the reviewer session (`auto_merge`) | The reviewer approved the PR |
| `dispatch:merging` | issuewright (`auto_merge`) | issuewright enabled auto-merge on the approved PR, which waits for its checks; if the PR conflicts, issuewright resumes its session to rebase |

When issuewright claims an issue, it swaps `dispatch:ready` for `dispatch:running` and comments with the slot, branch and session name. To release an issue again after `dispatch:needs-input`, answer in the issue and set `dispatch:ready`. The next session reads the comments; if the branch was already pushed, it continues from that branch.

## Scheduling

Every `poll_interval_secs`, issuewright reads all open issues with a `dispatch:*` label in the configured repositories and orders them in five steps:

1. **Candidates.** Open `dispatch:ready` issues. Issues labeled `status:blocked` are skipped, and so are epics (issues with sub-issues, or labeled `type:epic`); only an epic's sub-issues are scheduled. Sub-issues count only when linked as GitHub sub-issues, not when the epic merely lists them. For a released `type:epic` issue without sub-issues, issuewright comments once with how to fix it.
2. **Dependencies.** An issue is runnable only when every blocker is done. Blockers come from GitHub's native "blocked by" relations (cross-repository included) and from issue references under a `Dependencies / Blockers` heading in the issue body. References elsewhere in the body don't count, but every reference in that section does, including ones mentioned only as background. The exception is a section that opens with "None" (or "n/a", "No blockers", "Keine"): it declares no blockers, and the references after it are context. A blocker is done when it is closed as completed, or (with `blocker_done = "merged_or_closed"`, the default) when a PR that *closes* it has merged into its default branch. A PR that only mentions it (`Part of #…`) doesn't count. A blocker that can't be resolved, such as an unreadable repository or a missing issue, keeps the dependent waiting, and issuewright comments on the dependent once.
3. **Started epics first.** An epic counts as started once one of its sub-issues is closed, has a merged PR, carries `dispatch:running`/`review`/`needs-input`, or `status:in-progress`/`status:review`. Runnable sub-issues of started epics go before everything else. Started epics are ordered by the epic's priority, then by start time: the earliest close among its sub-issues or issuewright's first claim; GitHub doesn't say when a person started one, so the epic's creation time stands in. With `max_started_epics = N`, no sub-issue of an unstarted epic starts while N started epics still have open sub-issues.
4. **Order inside a group.** The epic's sub-issue order, then priority (`priority:P0`…`P4`, or the legacy `P0`…`P4`), then age. One thing goes before the sub-issue order and age, though not before priority: how many ready issues wait on an issue, directly or through others. The issue that frees the most work comes first, so a blocker with a long chain behind it starts before an unrelated issue of the same priority. The queue shows the count as `unlocks N`.
5. **Capacity.** Slots are filled from the top, within `max_sessions` overall and each repository's `slots`. A repository with no free slot doesn't hold back issues of other repositories. With `max_in_ci = N` on a repository, it claims no new issue while N of its issues wait for CI (`dispatch:ci`). A session finishes faster than CI does, so without a limit open PRs pile up behind the runners, conflict with each other as they merge, and run CI again after each rebase. CI fixes, rebases and reviews still start, since they move PRs out of that queue. Two more things hold issues back here: an issue whose session failed to start waits before its next attempt (see [When issuewright itself fails](#when-issuewright-itself-fails)), and no new session of any kind starts while resources are short. A ready `priority:P0` issue goes first: each poll claims it before CI fixes, rebases and reviews take the free slots, and neither `max_in_ci` nor a hold for load keeps it back. Its blockers, `max_sessions`, the repository's slots and a hold for disk space still do; a running session is never stopped for it.

Sub-issues of one epic with no declared dependency between them may run in parallel. Declare a dependency when one builds on the other's result, not merely because both touch the same files. A sub-issue never waits on its own parent, even if its dependencies section names it: the parent closes only after its sub-issues.

`issuewright run --dry-run` and `issuewright status` print every issue with the reason it is waiting, for example `waiting on app#12 (open, no merged PR)`, `epic app#40 not started, max_started_epics (1) reached`, `no free slot in app` or `6 PRs wait for CI, max_in_ci (6) reached`.

### Watching issuewright

`issuewright watch` is a screen to keep open beside the daemon. It reads what the daemon writes (`status.json`, the slot locks, the log) and the slots' transcripts, asks tmux which windows are alive, calls no GitHub API and changes nothing. It shows:

- the tabs (`Live`, `Stats`), the last poll, slots in use and sessions asking you;
- the ready work by what it waits on: **qualified** (nothing in its way, it starts as slots free up), **behind work in flight** (it waits only on blockers that move on their own), and their sum, the work that gets done **without you**. Then what's **held** by stuck chains, the issues that **need you** (`dispatch:needs-input`), the ones **parked** with `status:blocked`, and those **in flight** (running, in CI, in review, merging);
- today's numbers from the log: sessions started, PRs handed to CI, CI failures, approvals, closed issues, and handbacks to you. Next to them is a sparkline of busy slots over the last day, one character per half hour;
- **1 Sessions:** the running sessions, those waiting for your answer first, then the oldest. A session waiting for you shows "asks you" with how long, and when issuewright will move its question to GitHub. The free slots share one line at the end, so the running sessions always fit;
- **2 Pipeline:** the work in flight, closest to merged first (`merging`, `approved`, `changes-requested`, `review`, `ci`, `running`), each with how long ago issuewright's log last said something about it, so what sits still stands out. `review (you)` marks a PR in a repository without `auto_merge`, which waits for you. The title counts each stage;
- **3 Needs you:** everything that won't move until you act, with what to do, the items that free the most ready issues first, then the oldest. `reply`: a session asks you in its window (`Enter` goes there). `answer`: an issue at `dispatch:needs-input`, with how long since it went to you. `review`: a green PR in a repository without `auto_merge`. `unblock`: the end of a stuck chain of blockers (see below). `frees N` counts the ready issues waiting on it;
- **Events:** the last lines of issuewright's log, in local time;
- **4 Stats**, a tab of its own: the two tables of [`issuewright stats`](#throughput-and-efficiency) for every day, what got done and how long it took, what it cost and what was left, with bars of sessions started, approvals and CI failures per day.

`issuewright status --tree` prints the chains of blockers as trees. Each tree starts at the issue a chain ends at and lists the issues waiting on it, directly or through others. `+#12` names an issue's other open blockers, and "also held by" names other ends it waits on. The trees whose end won't move until you act come first; these ends are the `unblock` items of **Needs you**:

| End | Meaning |
|---|---|
| open without a `dispatch:*` label | Nothing will start it: label it `dispatch:ready`, or drop the dependency. |
| closed as not planned | It never counts as done. Remove the dependency. |
| can't be looked up | Wrong reference, or a repository the token can't read. |
| `dispatch:needs-input` | Waits for your answer. |
| `status:blocked`, or ready but skipped | Parked, or excluded by the scheduler (an epic, another `dispatch:*` label). |
| deadlock | Ready issues that wait on each other, directly or through others. Remove one of the dependencies. |

Below them are the trees whose end moves on its own: a session or GitHub works on it, or it's ready and starts at the next free slot. Then come the ready issues that nothing holds back. Plain `issuewright status` lists the stuck ends only.

Keys:

| Key | What it does |
|---|---|
| `Tab`, `Shift+Tab`, `1`–`3` | Pick a pane. |
| `4` | The Stats tab; `Esc`, `Tab` or `1`–`3` go back. |
| `↑`/`↓`, `j`/`k`, `PgUp`/`PgDn`, `g`/`G` | Move the selection over the pane's issues. |
| `Enter` | Switch your tmux client to the selected issue's session window. `watch` must run inside tmux. Come back with the tmux prefix and `L`. |
| `o` | Open the issue or PR on GitHub. |
| `r` | Reload now (it reloads every 2 seconds anyway). |
| `q` | Quit. |

### Throughput and efficiency

`issuewright stats [--days 14] [--no-tokens]` prints two tables with one row per day, and a summary. They're counted from issuewright's log, the slots' transcripts and the backlog issuewright records at each poll:

| Column | Counts |
|---|---|
| started, reviews | implementing and reviewer sessions started |
| to CI, CI red, rebase | PRs handed to CI, CI failures issuewright resumed a session for, conflicts it resumed one to rebase |
| closed | issues closed while their session ran, by a merge or by hand |
| approved, changes | review verdicts |
| to you | issues handed back with `dispatch:needs-input` |
| errors | things issuewright tried that failed (claims, CI checks, merges) |
| lead p50 | median from an issue's first claim to its approval |
| CI p50 | median from handing a PR to CI to its result, queue included |
| read, output, out/approved | model tokens of the slots' sessions: everything read (input and cache), the output, and output per approval |
| qualified … in flight | the backlog at the day's last poll, as in `issuewright watch`. issuewright keeps it in `backlog.json` in its state directory, one sample an hour for 90 days; days before it recorded any show `–` |

The summary adds p90s, the share of PRs with a red CI round, the share of implementing sessions (fresh or resumed) that ended with you, and output tokens per approval. Each model turn counts once, even when issuewright copied its transcript to resume it in another slot. Reading all transcripts takes a few seconds; `--no-tokens` skips it. Neither the command nor the tab calls GitHub, so the time from approval to merge isn't included.

## Splitting and closing epics

**Splitting.** A session that finds its issue too big for one PR doesn't start it. It proposes parts (each with a goal, acceptance criteria and its dependencies on earlier parts) and asks you. In unattended mode it writes the proposal as a comment and sets `dispatch:needs-input`. Once you confirm, it creates the parts as sub-issues of the issue, in order. Each part gets the issue's labels (priority, type, area, ...) except `dispatch:*` and `type:epic`, plus `dispatch:ready`. The session then removes `dispatch:running` from the issue. issuewright sees an issue with open sub-issues and no `dispatch:running`, closes the session's window at once, and from the next poll on schedules the parts like any epic's sub-issues. The original issue is now their parent and is never claimed itself.

**Closing an epic.** When all sub-issues of an epic are closed, issuewright starts an epic-check session `<repo>#<number>-epic` (with `check_epics`, on by default). It covers epics that issuewright scheduled sub-issues of, and epics carrying a `dispatch:*` label. The session runs on the current default branch and can't commit or push. It checks each acceptance criterion of the epic against the merged code, PRs and sub-issues, and ticks the met checkboxes in the epic's body.
- All met: it comments the evidence and closes the epic.
- Some open: it proposes follow-up issues. Once you confirm, it creates them as new `dispatch:ready` sub-issues and releases the epic, which gets scheduled again. If you decline, or in unattended mode, it sets `dispatch:needs-input` with the open criteria. Set `dispatch:ready` on the epic to check it again.
- Open only until other work ships (a criterion that can only be checked live once another issue has landed): it adds that issue to the epic's blockers, comments which criterion waits on it, and releases the epic.

An epic isn't checked while it's `status:blocked`, or while one of its blockers is open: its native blocked-by links and its `Dependencies / Blockers` section count, as for any issue. When a check leaves the epic open, issuewright remembers what it saw (`epic_checks.json` in its state directory): the body, the labels it doesn't set itself, the sub-issues, and the blockers with whether each is done. It doesn't check the epic again until one of them changes, or you set `dispatch:ready` on it. The plan's notes say why an epic waits, for example `skipped: epic check of app#40 unchanged since 2026-11-27 08:00 UTC`.

The check's prompt is [`src/epic_check_prompt.md`](src/epic_check_prompt.md), or `epic_check_prompt_template`.

## Sessions

### Interactive mode

issuewright works in the tmux session `agents`. Window 0 follows issuewright's log. Each claimed issue gets a window named `<repo>#<number>`, running

```
claude --remote-control=<repo>#<number> --name <repo>#<number> --settings <generated> -- "<prompt>"
```

in the slot. Remote Control is on from the start, so the session appears under that name in the Claude app and at claude.ai/code. When it asks a question, you can answer from your phone. A `Notification` hook also runs `notify_command`.

The slot is freed when the `claude` process exits. An interactive `claude` doesn't exit on its own, so every session gets a `Stop` hook (`issuewright stop-hook`) that runs after each of its turns. It looks up the session's issue (or the PR a reviewer reviews) on GitHub, and once the session's part is over, it closes the session's window:

- implementing and epic-check sessions: the issue left `dispatch:running` (it went to `dispatch:ci` with the PR up, or to `dispatch:needs-input`, or was split), or was closed;
- reviewer sessions: the verdict is set (`dispatch:approved`, `dispatch:changes-requested` or `dispatch:needs-input`), or the subject left `dispatch:review` or was closed.

The next poll frees the slot. The conversation stays resumable with `claude --resume` in the slot's directory. If the hook can't decide (GitHub unreachable, for example), the window stays open and the rules below still close it: `issuewright status` lists such a session as "done, waiting to be closed", and with `close_done_after_secs`, issuewright closes the window itself that long after the issue reached `dispatch:ci` or `dispatch:review` (or closed). A session whose issue is closed (its PR merged with `Closes #N`, or closed by hand), or was split into sub-issues, has nothing left to do: issuewright closes its window at the next poll, without a grace period and whether `close_done_after_secs` is set or not. If a session ends while its issue is still `dispatch:running`, issuewright sets `dispatch:needs-input` and comments.

### Unanswered questions

A session that asks you something holds its slot until you answer. A turn that ends while the session still has work means it's waiting for you: its `Stop` hook marks the slot as waiting, and a `UserPromptSubmit` hook (`issuewright prompt-hook`) clears the mark when anyone answers, in the terminal or the Claude app. Input the hook doesn't see, such as a `!` shell command typed into the session, restarts the wait too: issuewright reads the newest `user` entry of the session's transcript. `issuewright watch` shows such a session as "asks you".

In interactive mode a session asks here first for everything it would otherwise hand back at once: a decision, a dependency the issue doesn't declare, a CI failure it didn't cause and a re-run won't clear, a rebase conflict that needs a decision. It comments its findings on the issue and waits. Only when nobody answers does the timeout move the question to GitHub.

When nobody answers within `question_timeout_secs` (default 900; 0 waits forever), issuewright types a message into the session: commit and push the work worth keeping, write the question as a comment on the issue with everything needed to answer it, set `dispatch:needs-input`, and stop. The Stop hook then closes the window, and the slot is free for other work.

Answer on GitHub and set `dispatch:ready`. The next claim resumes that conversation instead of starting a fresh one: in its old slot if that one is free, otherwise in another one with the transcript copied. It starts on the branch it pushed, with a message to read the answer and carry on ([`src/answered_prompt.md`](src/answered_prompt.md)). The claim comment says so. After 7 days, or without the transcript, a fresh session starts as usual. Reviewer and epic-check sessions move their question the same way, but a later release starts them afresh.

### Waiting for CI without a slot

A session doesn't wait for CI. Once it has pushed and opened the PR, it sets `dispatch:ci` and exits, so its slot goes to other work while the checks run. issuewright starts every session with a known `--session-id` and remembers the last implementing session of each issue (`state/ci.json`). At every poll it reads the check rollup of each `dispatch:ci` issue's PR head commit:

- **Running:** nothing happens until the next poll.
- **Green:** issuewright comments and swaps `dispatch:ci` for `dispatch:review`, and the review starts as usual. A PR that reports no checks at all for 3 minutes counts as green: the repository has no CI.
- **Red:** once the repository has a free slot, issuewright resumes the same conversation (`claude --resume <id>`) on the PR's branch, with the failed checks and their links as its first message, and sets `dispatch:running`. It prefers the session's old slot; if that one is busy, it copies the transcript into a free slot's project directory and tells the session about the new directory. The session fixes the failure, pushes, and sets `dispatch:ci` again. After three red rounds, or for a failure it didn't cause, it asks you instead: in the session in interactive mode (see [Unanswered questions](#unanswered-questions)), with `dispatch:needs-input` in unattended mode.

CI fixes go before reviews and new claims. If there is no session to resume (it started before issuewright recorded session ids, or its transcript is gone), or no open PR closes the issue, issuewright sets `dispatch:needs-input` and comments why.

The first session in a new slot stops at Claude Code's "trust this folder" prompt. Trust is stored per path and slots are persistent, so you answer it once per slot.

### Unattended mode

`--mode unattended` (headless `claude -p` with a budget and an explicit tool allowlist) is not implemented yet.

## Review and merge

With `auto_merge = true` on a repository and a [review app](#the-review-app) installed on it, a `dispatch:review` issue doesn't wait for you:

1. Once the issue's own session is gone (it exited, or was closed by you or by `close_done_after_secs`) and a slot is free, issuewright looks up the open PR in the same repository that closes the issue (`Closes #N`) and targets the default branch. It starts a **reviewer session** `<repo>#<number>-review` on the PR's branch, pinned to the PR's current head commit. Reviews go before new claims.
2. The reviewer is a fresh session with its own prompt ([`src/review_prompt.md`](src/review_prompt.md), or `review_prompt_template`) and `review_model`/`review_effort`. It acts as the review app, reads the issue, epic and diff, and runs the affected tests. It can't commit or push. It then gives one verdict, as a PR review and a label:
   - an approving review plus `dispatch:approved`: issuewright checks that the app's latest review approves exactly the reviewed commit, then runs `gh pr merge --auto --<merge_method> --match-head-commit <sha>` **with its own token**, swaps the labels for `dispatch:merging` and comments. GitHub merges once the branch protection rules pass (at once if there are none), and the merge closes the issue. GitHub refuses auto-merge on a PR that can merge already (its checks passed before the verdict, the usual case); issuewright then merges it directly, still with `--match-head-commit`, removes `dispatch:approved` and `dispatch:review` without setting `dispatch:merging`, and says so in the comment.
   - a review requesting changes plus `dispatch:changes-requested` (with `dispatch:review` removed), and a comment on what has to change, when the PR's author can fix every finding without a decision from you. Once the repository has a free slot, issuewright resumes the session that wrote the PR, as it does when CI fails, with a brief to address the review ([`src/changes_requested_prompt.md`](src/changes_requested_prompt.md)), and sets `dispatch:running`. The session fixes the findings (or explains on the issue why one is wrong), pushes and sets `dispatch:ci`; CI and a fresh review follow. After three such rounds without approval, or without a session to resume, issuewright sets `dispatch:needs-input` instead and comments. The count starts over once the issue went to you.
   - a review requesting changes plus `dispatch:needs-input` (with `dispatch:review` removed), and a comment on what has to change, when a finding needs production, a decision the issue leaves open, or a person for another reason. Answer it like any other `needs-input`. Setting `dispatch:ready` lets a session fix it on the same branch, and that session's `dispatch:review` triggers a fresh review.
   - a review comment, blocked-by links to the issues that must land first, and `dispatch:ready` (with `dispatch:review` removed). The reviewer uses this when the PR is fine but must not merge before other work does, such as a PR it builds on, or behaviour it documents that isn't live yet. The issue then waits like any other ready issue with blockers, and `issuewright status --tree` shows it in its blockers' trees. Once they are done, a session continues the open PR: it rebases onto the default branch, re-checks the PR against what landed, pushes and sets `dispatch:ci`, and CI and a fresh review follow. Nobody has to step in. A dependency that has no issue of its own still goes to `dispatch:needs-input`.
3. While an issue is `dispatch:merging`, issuewright checks at every poll whether its PR still merges. GitHub never merges a PR that conflicts with the default branch, so the issue would wait forever. issuewright turns the PR's auto-merge off, so the rebased commit can't merge on the old approval before anyone reviewed it, and resumes the session that wrote the PR with a rebase brief ([`src/rebase_prompt.md`](src/rebase_prompt.md)), as it does when CI fails (see [Waiting for CI without a slot](#waiting-for-ci-without-a-slot)). The session rebases onto the default branch, resolves the conflicts keeping both sides' work, pushes with `--force-with-lease`, and sets `dispatch:ci`. After CI, the rebased PR gets a fresh review. If the conflict needs a decision the issue doesn't make, the session sets `dispatch:needs-input` instead. Without a session to resume, issuewright sets `dispatch:needs-input` and comments.
4. A reviewer that ends without a verdict, and a merge that fails (the branch moved since the review, auto-merge isn't allowed in the repository, conflicts), become `dispatch:needs-input` with a comment. Nothing is merged. To review again, replace `dispatch:needs-input` with `dispatch:review`.

Safeguards:
- Sessions still can't merge (`gh pr merge` stays denied), and the session token never needs merge rights. Only issuewright merges, and only for a reviewer session it started itself, and only with the app's approving review of the reviewed commit. A `dispatch:approved` set by anyone else is removed when the review starts, and a label without the app's approval merges nothing.
- The merge is pinned to the reviewed commit, so a push after the review makes it fail instead of merging unreviewed code.
- `--auto` leaves the last word to GitHub: required checks and reviews in the branch protection still apply. Enable "Allow auto-merge" in the repository settings. Without required checks, an approved PR merges immediately.
- `merge_method` (default `squash`) must be a method the repository's settings and every ruleset on the default branch allow. GitHub enables auto-merge with any method, but never merges with one a ruleset forbids. issuewright warns about a mismatch at start, and instead of enabling such an auto-merge it sets `dispatch:needs-input` with a comment.
- A `dispatch:review` issue without an open PR that closes it (`Closes #N`) and targets the default branch goes to `dispatch:needs-input` with a comment, because nothing would ever be reviewed or merged for it. This happens after partial PRs (`Part of #N`), which sessions must not open: one PR per issue, or a split into sub-issues. A draft PR waits; issuewright comments once.
- `dispatch:review` on a pull request that closes an issue is moved, at the next poll and in every watched repository, to the open issues it closes, with a comment on the PR: the issue's review covers it.
- A pull request that closes no issue, such as a partial `Part of #N` PR, is reviewed on its own when it carries `dispatch:review` itself. The reviewer ([`src/pr_review_prompt.md`](src/pr_review_prompt.md), or `pr_review_prompt_template`) reads the PR and the issues it refers to, and sets its verdict label on the PR. Approval, merge and `dispatch:needs-input` work as for an issue, with labels and comments on the PR; the issues it is part of stay open. Only PRs from a branch of the repository into its default branch are reviewed; others go to `dispatch:needs-input`. A draft waits.

A reviewer session takes a slot like any other session. It exits once it has set its verdict.

At start, issuewright prints a warning for each `auto_merge` repository where this can't work as intended: no `[review_app]` configured or the app not installed there (reviews are disabled), the default branch requiring approvals while the app has only read access to Contents (its approval doesn't count), "Allow auto-merge" off (every merge fails), or no branch protection or ruleset on the default branch that requires status checks or reviews (an approved PR merges at once, without waiting for CI). Reviews start only in repositories the app was installed on when issuewright started.

### The review app

GitHub doesn't count an approval from the PR's author, and sessions open PRs under your account. So reviewers act as a GitHub App, whose approval counts toward required reviews. Without `[review_app]`, reviews are disabled.

The quick way:

```
issuewright setup-app                # in your personal account
issuewright setup-app --org acme     # or in an organization
```

It opens GitHub's "new app" page with the name (`<login or org>-issuewright`, or `--name`), the permissions below and a disabled webhook filled in. Click **Create GitHub App**. GitHub hands the app's id and private key back to issuewright, which saves the key next to the config (mode 600) and adds `[review_app]` to the config. Then it opens the install page, where you pick the repositories with `auto_merge`. It never replaces an existing `[review_app]`.

It needs a browser that reaches `127.0.0.1` on this machine. On a remote machine, forward the port it prints (`ssh -L <port>:127.0.0.1:<port> <host>`, with a fixed `--port` if you like). If the redirect at the end still doesn't arrive, paste the URL the browser shows into the terminal.

Or by hand:

1. Create a GitHub App (your org's Settings → Developer settings → GitHub Apps → New). Name it, for example, `<org>-issuewright-reviewer`. Disable the webhook. Repository permissions:
   - Pull requests: read and write (to approve or request changes)
   - Contents: read and write. GitHub counts an approval toward required reviews only from a reviewer with write access, and for an app that is this permission. With read-only, its approval shows on the PR but the PR stays blocked. The reviewer can't use the write access to merge unreviewed code: its sessions can't `git push` or `git commit`, and issuewright merges only the exact commit the app approved.
   - Issues: read and write (labels and comments)
   - Checks, Commit statuses, Actions: read-only (CI results and logs)
   - Metadata: read-only (required)
2. Generate a private key and store it outside the slots, for example in `~/.config/issuewright/reviewer.pem` with mode 600. Sessions can't read it: issuewright adds the file to every session's deny list.
3. Install the app on the repositories with `auto_merge`.
4. Configure it:

```toml
[review_app]
app_id = 123456
private_key = "~/.config/issuewright/reviewer.pem"
# or: private_key_command = "pass show github/issuewright-reviewer"
```

issuewright signs a short-lived JWT with the key and gives each reviewer session an installation token limited to that one repository. The token is valid for an hour, so a review that takes longer loses its GitHub access. issuewright itself keeps merging with its own token.

## Slots

Each repository has `slots` persistent git worktrees under `<home>/slots/<repo>/<i>`, created from one `--no-checkout` clone (`.base`). On each claim, issuewright resets the slot:

- remove git lock files left behind: any in the slot's own `.git/worktrees/<i>/` (a git command killed with its session leaves `HEAD.lock` there), those in the shared `.git` older than 10 minutes, and the read-only `config.lock` a session's sandbox leaves. Each one would fail the next git command with "File exists".
- `git fetch`
- detach any other idle slot that still has the branch checked out (git allows a branch in one worktree at a time), so an issue claimed again, or its review, can land in a different slot. A slot whose session is still running keeps it, and the claim waits.
- `git switch -C <branch> origin/<default branch>` (or `origin/<branch>` if the branch was already pushed). A branch whose remote tip is the head of an already merged PR isn't continued: after a squash merge it would build on old code and its push would be rejected. The work then starts from the default branch on `<branch>-2` (`-3`, ...).
- set the branch's upstream to `origin/<branch>` (with `push.default = current`), because sessions can't: the sandbox never lets them write `.git/config`, so `git push -u` fails. A plain `git push` works.
- `git reset --hard`
- `git clean -fdx -e target/`

`target/` survives the reset, so builds stay warm. `CARGO_TARGET_DIR` points at the slot's `target/`, `TMPDIR` at `<home>/slots/<repo>/tmp-<i>`, and `CARGO_HOME` is shared. With `target_dir_max_gb`, a slot's `target/` is removed before its next claim once it grows past that size. Branches are named by `branch_template`, by default `{kind}/{number}-{slug}`, where `{kind}` is `fix` for `type:bug`, `docs` for `type:docs`, `chore` for `type:chore`/`type:spike`, and `feat` otherwise.

**Signed commits.** The signing key usually lives under `~/.ssh`, which sessions can't read. Point `session.signing_key` at a private SSH key without a passphrase, registered on GitHub as a signing key:

```toml
[session]
signing_key = "~/.ssh/id_ed25519_signing"
```

issuewright then runs an `ssh-agent` of its own that holds only that key, at `<home>/signing/agent.sock`. The agent starts and stops with issuewright, at the same path each time, so sessions that outlive a restart sign again once the next issuewright is up. Sessions get the agent as `SSH_AUTH_SOCK` and the key's public half inline (`user.signingkey = "key::ssh-ed25519 …"`, `gpg.format = ssh`, `commit.gpgsign = true`). git signs through the agent and never opens a file under `~/.ssh`. The settings go into the slot's git config, for `session rebase` too, and into the session's environment (`GIT_CONFIG_COUNT`, …). The environment wins over your own git config, `includeIf` sections included, in every repository the session touches, clones it makes itself as well. Each commit then shows as verified on GitHub, and PRs merge into branches that require signed commits with any merge method. `sign_commits = false` on a repository leaves its commits unsigned. On Linux the sandbox must reach the agent's socket, which needs `allowAllUnixSockets` (see [Changing the permissions](#changing-the-permissions)).

Whoever can reach the agent can have it sign anything with that key, so use a key that signs only, and nothing else: a session that pushes as you can already author commits as you.

Without `signing_key`, the sessions' commits are unsigned, and the environment sets `commit.gpgsign = false`, so that a commit in a clone the session made itself doesn't fail on a signing key the sandbox hides.

## Guardrails

Sessions must not reach production. Scrubbing the environment alone doesn't achieve that: `HOME` stays, so `kubectl` would still find `~/.kube/config` and `ssh` would still find `~/.ssh`. issuewright therefore layers:

1. **Sandbox.** Bash runs in Claude Code's sandbox (`failIfUnavailable`, no unsandboxed escape hatch):
   - Credential paths can't be read: `~/.ssh`, `~/.kube`, `~/.aws`, gh's `hosts.yml`, cargo/npm credentials, and so on, plus `session.deny_read`.
   - The home directory is read-only, except the slot, its temp dir, the base clone's `.git` and `CARGO_HOME`. `CARGO_HOME/bin` and the directory of issuewright's own binary stay read-only even so: what is in them runs outside the sandbox, as issuewright's hooks do.
   - Network access goes only through a proxy that allows `allowed_domains`: GitHub and the package registries by default. Plain `ssh` and connections to internal hosts fail.
2. **Read rules.** `Read(...)` deny rules block the same paths for Claude's file tools.
3. **Deny rules.** Built in: `gh pr merge`, `gh workflow run`, `gh run rerun`, `gh api …dispatches…`/`…merge…`, and pushes to `main`/`master`, because a merge or a workflow dispatch can deploy. Add your own, such as `kubectl`, `helm` or `ansible-playbook`, with `session.deny` and per repository with `deny`.

   Allow rules let a session's routine steps through without a prompt, and in `auto` mode without the classifier, which otherwise refuses some of them now and then: reading CI (`gh pr checks`, `gh run view`, `gh api repos/…/actions/…`), viewing, creating and commenting on PRs and issues, `gh issue edit`, and `git push --force-with-lease`. The deny rules still win over them. `session.builtin_allow = false` drops them. A rule matches only a command on its own: one with a heredoc, `cd`, `&&`, `;` or a pipe still goes to the classifier. So the system prompt of implementing sessions tells them to create issues, in their own repository or another one (a follow-up, or the console or SDK change an API change calls for), as `gh issue create --repo <owner/name> … --body-file <file>` alone. Earlier sessions wrote the body in a heredoc, and the classifier refused about one in eight of those calls, most often a ticket in another repository, as information leakage.
4. **issuewright's session commands.** A few steps a session needs, the sandbox or the deny rules keep it from doing. issuewright offers them as `issuewright session …`, which runs outside the sandbox and does one fixed thing to the session's own slot:
   - `session rebase` (then `continue` or `abort`): rebases the branch onto the default branch. Inside the sandbox a rebase fails halfway once the default branch changed a file the sandbox always protects, such as `.claude/skills/`.
   - `session reset`: discards uncommitted changes, for a worktree a failed git command left half changed.
   - `session rerun-ci <run>`: re-runs the failed jobs of a finished `pull_request` or `push` run of the session's own branch, at most up to its third attempt. A re-run of a deploy, or of anything on the default branch, stays impossible.

   issuewright lists `<issuewright> session *` in the sandbox's `excludedCommands` and the allow rules, and tells implementing sessions about it in their system prompt. Reviewers don't get it. The session writes the command line and its environment, so neither is trusted: each session gets a key (`ISSUEWRIGHT_SESSION_KEY`), and the command looks the key up in records issuewright writes to `~/.local/state/issuewright/sessions` (home from passwd, not `$HOME`), which every session's sandbox denies reading. git then runs in the slot that record names, with a fixed environment and with hooks off.
5. **Environment.** Only `session.env_passthrough` (`PATH`, `HOME`, `TERM`, …) plus what issuewright sets: `GH_TOKEN`, an empty `GH_CONFIG_DIR`, `CARGO_HOME`, `CARGO_TARGET_DIR`, `TMPDIR`, `CLAUDE_CODE_SUBAGENT_MODEL` (default `sonnet`) and the git settings for [signed commits](#slots) (`GIT_CONFIG_COUNT`, …), then `[env]` and the repository's `env`. No SSH agent unless `ssh_agent = true`; with `signing_key`, `SSH_AUTH_SOCK` is issuewright's signing agent.

The GitHub token is the remaining risk: it can do whatever its account can. Give sessions a fine-grained token limited to the watched repositories (contents, issues and pull requests read-write; Actions and commit statuses read-only, so sessions can read CI results and logs; no administration) with `session.gh_token_command`.

`issuewright show-session <owner/name>` prints the settings and environment a session in that repository gets (`--review` for a reviewer session).

### Changing the permissions

Nothing above is fixed in the binary; everything can be changed in the config. Sessions also read your own `~/.claude/settings.json`, because `HOME` is kept, so your usual allow rules and default mode apply. issuewright's `--settings` file goes on top of them, and Claude Code merges permission lists across files.

| To... | Set |
|---|---|
| allow more without prompts | `session.allow` / per repo `allow`, e.g. `["Bash(cargo *)", "Bash(npm run *)"]` |
| pick the permission mode | `session.permission_mode` / per repo `permission_mode` (`auto`, `acceptEdits`, ...) |
| add deny rules, credential paths, domains | `session.deny`, `deny_read`, `allowed_domains` (and per repo `deny`, `allowed_domains`) |
| drop a built-in list | `session.builtin_deny = false`, `builtin_deny_read = false`, `builtin_allowed_domains = false`, `builtin_allow = false` |
| turn the sandbox off | `session.disable_sandbox = true` |
| change anything else | `session.settings` / per repo `settings`: raw Claude Code settings as a TOML table, or `settings_file` (JSON) |

The raw settings are deep-merged over what issuewright generates, in this order: `session.settings`, `session.settings_file`, the repo's `settings`, the repo's `settings_file`. Tables merge key by key; anything else, lists included, replaces. For example:

```toml
[session.settings.sandbox]
allowUnsandboxedCommands = true        # let Claude retry a failing command outside the sandbox, with a prompt

[[repo]]
name = "acme/app"
[repo.settings.sandbox.network]
allowedDomains = ["*.example.org", "github.com"]   # replaces the generated list for this repo
```

Check the result with `issuewright show-session <owner/name>` before relying on it.

## The prompt

The built-in prompt is [`src/prompt.md`](src/prompt.md). Replace it with `prompt_template`, which takes the variables `{repo}`, `{number}`, `{title}`, `{mode}`, `{branch}`, `{branch_note}`, `{epic}` and `{blockers}`. It tells the session to:

- read the issue, the epic and the blockers' PRs;
- if the issue is really an epic, not implement it but propose labeling it `type:epic` and linking its parts as sub-issues, and do so only once you confirm;
- if the issue is too big for one PR, propose a split, and create the parts as `dispatch:ready` sub-issues only once you confirm (see [Splitting and closing epics](#splitting-and-closing-epics));
- follow the repository's own rules;
- build and test only what the change affects;
- push with a plain `git push`, open a PR with `Closes #N`, comment, set `dispatch:ci` and exit without waiting for CI; when issuewright resumes it with failed checks, fix them (up to three rounds) and set `dispatch:ci` again;
- if it finds an undeclared dependency, or a step that needs production, comment on the issue and set `dispatch:needs-input` instead.

## State and recovery

State lives in `<home>/state`:

- one lock file per busy slot;
- `history.json`, recording when issuewright first claimed a sub-issue of each epic;
- the one-time comments already posted;
- `status.json` with the last plan;
- `ci.json`: the last implementing session of each issue, to resume it when CI fails, and when issuewright first saw each `dispatch:ci` PR's head commit.

A slot's temp directory (`tmp-<i>`) is emptied before each session. Read-only directories, such as Go's module cache, are made writable first. Files issuewright can't remove at all, such as a socket directory a container's user owns, move to `slots/<repo>/.trash/`, and issuewright retries at every reset; remove what stays there as root.

### When issuewright itself fails

issuewright logs every error, and some come back poll after poll: a slot it can't clear, a full disk, a broken token. When an error of the same kind (numbers and commit hashes don't count, so the same failure for different issues is one kind) shows up in `alarm_after_polls` consecutive polls (3 by default), issuewright raises an alarm. It logs `alarm: failing for 3 min (12 like it): …`, runs `alarm_command` (by default a 15-second tmux message), and shows the alarm in red in `issuewright watch` and `issuewright status`. When the error stops, it logs `alarm cleared` and runs `alarm_command` again. `{message}` in the command is the text with quotes removed, and `ISSUEWRIGHT_ALARM` holds it as is, so a command can send it on, for example to your phone.

An issue whose session fails to start, or whose CI or merge check fails, isn't retried every poll. The first retry is the next poll, because most such failures are gone by then, then after 2, 4, 8 and at most 30 minutes. Meanwhile it shows as "failed N times, next try at HH:MM", and the slot goes to the next issue.

New sessions (claims, reviews, CI fixes, rebases, epic checks; for load, all but `priority:P0` claims) wait while the disk of `home`, or of a path in `disk_paths`, has less than `min_free_disk_gb` free (10 by default), or while the five-minute load average is above `max_load` (off by default). Running sessions go on. issuewright logs when the hold starts and ends, the plan shows it as the wait reason, and a hold that lasts raises an alarm too.

issuewright reloads its config file before each poll when the file changed, and logs the settings that changed (`config reloaded: max_sessions, repo owner/name: auto_merge`). Changes apply from that poll on; sessions already running keep the settings they started with. A file that doesn't load, or one that removes a repository while a session of it holds a slot, keeps the previous config and logs why. `home`, `tmux_session` and `gh_user` change only on restart.

Only one issuewright runs per home. At start, issuewright frees locks whose window is gone and sets `dispatch:needs-input` on `dispatch:running` issues that have no live session. SIGTERM/SIGINT stops new claims; tmux windows stay open.

## Development

```sh
cargo test                     # scheduler, parsing, settings
ISSUEWRIGHT_LIVE_REPO=owner/name ISSUEWRIGHT_LIVE_LABEL=some-label \
  cargo test live -- --ignored --nocapture   # the GraphQL queries against GitHub
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every pull request and on pushes to `main`, and a PR merges only when it passes:

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
- `cargo test` on the minimum supported Rust version, 1.89 (`rust-version` in `Cargo.toml`)
- `cargo package`, which builds the crate as crates.io gets it, and `cargo doc` without warnings
- `cargo deny check`: advisories, licenses, duplicate and unknown-source dependencies ([`deny.toml`](deny.toml))

Run the first three before pushing. Dependabot proposes dependency and Actions updates weekly.

### Releasing

1. Bump `version` in `Cargo.toml`, run `cargo build` to update `Cargo.lock`, and merge that through a PR.
2. Tag the merge commit on `main` and push the tag: `git tag v0.2.0 && git push origin v0.2.0`.

The tag starts [`.github/workflows/release.yml`](.github/workflows/release.yml). It checks that the tag matches the version and is on `main`, runs the CI gates on the tagged commit, builds static Linux binaries (x86_64, aarch64), publishes the crate to crates.io, and creates a GitHub Release with the binaries, their SHA-256 checksums and generated notes. crates.io trusts the workflow through [Trusted Publishing](https://crates.io/docs/trusted-publishing), so no token is stored in the repository. A PR that changes the workflow or the manifest runs it as a dry run, up to the binaries.

The scheduler (`src/scheduler.rs`) is a pure function from a snapshot of issues to an ordered plan, and is unit-tested without GitHub.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
