# How issuewright works

Three views of the same process: the life of one issue, what one poll does, and how the scheduler picks the next issue. The [README](../README.md) has the details behind each step.

## The life of an issue

Rounded boxes are steps, diamonds are decisions, and the label after each step is what the issue carries next. Dashed boxes are optional: the fallback auto-close (`close_done_after_secs`) and review and merge (`auto_merge`).

```mermaid
flowchart TD
    A(["You label the issue dispatch:ready"]) --> B{"Scheduler: runnable?<br/>(see Scheduling)"}
    B -- "no: waits, reason in issuewright status" --> B
    B -- "yes, slot free" --> C["issuewright claims it<br/>dispatch:running<br/>resets a slot, opens a tmux window"]
    C --> D["Session reads issue, epic, blockers"]
    D --> E{"Is the issue<br/>really an epic?"}
    E -- "yes" --> E1["Proposes type:epic and sub-issue links,<br/>links them once you confirm"] --> NI
    E -- "no" --> SP{"Too big<br/>for one PR?"}
    SP -- "yes" --> SP1{"You confirm<br/>the proposed split?"}
    SP1 -- "no" --> NI
    SP1 -- "yes" --> SP2["Creates the parts as sub-issues<br/>same labels + dispatch:ready<br/>removes dispatch:running"]
    SP2 --> SP3["issuewright closes the session at once;<br/>the issue is now an epic"]
    SP3 --> A2(["Each part runs this flow<br/>from dispatch:ready"])
    SP -- "no" --> F{"Undeclared dependency,<br/>production step or<br/>open decision?"}
    F -- "yes" --> NI
    F -- "no" --> G["Implements, builds and tests what changed"]
    G --> H["git push (plain, upstream preset)<br/>PR with Closes #N"]
    H --> H1["Comments the PR link<br/>dispatch:ci"]
    H1 --> K["Stop hook after the turn<br/>sees the issue left dispatch:running<br/>and closes the window; slot freed"]
    K -. "hook couldn't decide:<br/>you close it, or<br/>close_done_after_secs elapsed" .-> K
    K --> I{"issuewright polls the PR's checks:<br/>CI green?"}
    I -- "still running" --> I
    I -- "red, and a slot is free" --> I1["issuewright resumes the same conversation<br/>(another slot: transcript copied)<br/>with the failed checks; dispatch:running"]
    I1 --> I2{"Own failure,<br/>under 3 rounds?"}
    I2 -- "yes" --> I3["Reads failed logs, fixes, pushes"] --> H1
    I2 -- "no" --> NI
    I -- "green, or no checks<br/>after 3 minutes" --> M["issuewright comments,<br/>dispatch:review"]

    NI["dispatch:needs-input<br/>comment says why"] --> NI2(["You answer in the issue<br/>and set dispatch:ready"]) --> A

    M --> N{"auto_merge on the repo<br/>and review app installed?"}
    N -- "no" --> O(["You review and merge the PR"])
    N -. "yes" .-> R

    subgraph RM ["Review and merge (auto_merge)"]
        R["issuewright starts a reviewer session<br/>on the PR's head commit,<br/>acting as the review app"] --> S{"Verdict"}
        S -- "approving review<br/>+ dispatch:approved" --> S1{"App's latest review<br/>approves that commit?"}
        S1 -- "no" --> NI3
        S1 -- "yes" --> T["issuewright: gh pr merge --auto<br/>--match-head-commit, own token<br/>dispatch:merging<br/>(already green: merged at once)"]
        S -- "review requesting changes<br/>the author can fix" --> C1["dispatch:changes-requested<br/>findings on the PR and issue"]
        C1 --> C2{"Under 3 rounds, and<br/>a session to resume?"}
        C2 -- "yes, a slot is free" --> C3["issuewright resumes the session that wrote it<br/>with the review; dispatch:running"]
        C3 --> H1
        C2 -- "no" --> NI3
        S -- "review requesting changes<br/>that need a person" --> NI3["dispatch:needs-input<br/>findings on the issue"]
        S -- "fine, but other issues<br/>must land first" --> S2["Review comment, blocked-by links<br/>to those issues, dispatch:ready"]
        S -- "none, session ended" --> NI3
        T --> U{"Merge accepted?"}
        U -- "no: branch moved,<br/>auto-merge off, conflict" --> NI3
        U -- "yes" --> V["GitHub merges once<br/>required checks pass"]
        V -- "PR conflicts with<br/>the default branch" --> V1["issuewright turns auto-merge off,<br/>resumes the session that wrote it<br/>to rebase; dispatch:running"]
        V1 --> V2["Session rebases, resolves,<br/>force-pushes with lease<br/>dispatch:ci: CI, then a fresh review"]
    end

    S2 -- "waits on its blockers; then a session<br/>rebases the open PR, re-checks it" --> B
    O --> W(["PR merged, issue closed by Closes #N"])
    V --> W
    W --> X["Any window still open for the issue<br/>is closed at the next poll"]
    W --> EP{"Part of an epic, and<br/>all its sub-issues closed?"}
    EP -- "no" --> EP0(["Done"])
    EP -- "yes" --> EC["issuewright starts an epic check<br/>dispatch:running on the epic"]

    subgraph EPC ["Epic check (check_epics)"]
        EC --> EC1{"All acceptance<br/>criteria met?"}
        EC1 -- "yes" --> EC2(["Ticks them, comments evidence,<br/>closes the epic"])
        EC1 -- "no" --> EC3{"You confirm<br/>follow-up issues?"}
        EC3 -- "yes" --> EC4(["Creates them as dispatch:ready<br/>sub-issues; they run this flow"])
        EC3 -- "no" --> EC5(["dispatch:needs-input<br/>open criteria on the epic"])
    end
```

## One poll

At start, issuewright warns about each `auto_merge` repository without the review app, with "Allow auto-merge" off, or without a rule requiring checks or reviews. Then, every `poll_interval_secs`, in this order. CI fixes and reviews claim slots before new issues: finishing work beats starting it.

```mermaid
flowchart TD
    P0(["Poll"]) --> P1["Snapshot: open issues with a dispatch:* label,<br/>their epics' sub-issues, blocker statuses<br/>(about 3 GraphQL points)"]
    P1 --> P2["Close finished windows<br/>issue closed or split: at once<br/>end label reached: after close_done_after_secs"]
    P2 --> P3["Free slots of ended sessions"]
    P3 --> P3a{"Ended while<br/>dispatch:running?"}
    P3a -- "yes" --> P3b["dispatch:needs-input + comment"]
    P3a -- "no" --> P4
    P3b --> P4
    P4["Merge PRs that live reviewer sessions approved"] --> P4a["Check CI of dispatch:ci PRs<br/>green: dispatch:review<br/>red: resume the session that pushed"]
    P4a --> P4b["Check dispatch:merging PRs<br/>conflicting: auto-merge off,<br/>resume the session to rebase"]
    P4b --> P4c["Sessions whose question waited question_timeout_secs:<br/>told to move it to GitHub and stop"]
    P4c --> P5["Start reviews (auto_merge repos<br/>with the review app installed)<br/>for dispatch:review issues with no session"]
    P5 --> P5a["Start epic checks for epics<br/>whose sub-issues are all closed"]
    P5a --> P6["Schedule dispatch:ready issues"]
    P6 --> P7["Comment once on unresolvable blockers<br/>and on type:epic issues without sub-issues"]
    P7 --> P7a["Claim free slots in scheduled order"]
    P7a --> P8["Write status.json for issuewright status"]
    P8 --> P0
```

## Scheduling

Which `dispatch:ready` issue gets the next free slot.

```mermaid
flowchart TD
    C0(["Open issue with dispatch:ready"]) --> C1{"status:blocked?"}
    C1 -- "yes" --> SK(["Skipped"])
    C1 -- "no" --> C2{"Epic?<br/>has sub-issues, or type:epic"}
    C2 -- "yes" --> SK
    C2 -- "no" --> C3{"Another dispatch:* label too?"}
    C3 -- "yes" --> SK
    C3 -- "no" --> C4{"Every blocker done?<br/>native blocked-by, and the<br/>Dependencies / Blockers section"}
    C4 -- "no, or unresolvable" --> WT(["Waits"])
    C4 -- "yes" --> C5{"Its epic started?"}
    C5 -- "yes" --> G1["Group 1, first:<br/>epic priority, epic start,<br/>issues waiting on it, sub-issue order,<br/>priority, age"]
    C5 -- "no" --> C6{"max_started_epics reached?"}
    C6 -- "yes, and it has an epic" --> WT
    C6 -- "no, or no epic" --> G2["Group 2:<br/>priority P0..P4 (none = last),<br/>issues waiting on it, age;<br/>an epic's sub-issues by issues<br/>waiting on them, then the epic's order"]
    G1 --> C7{"Free slot in its repo<br/>and under max_sessions?"}
    G2 --> C7
    C7 -- "no" --> WT
    C7 -- "yes" --> CL(["Claimed"])
```

A blocker is done when it is closed as completed or, with `blocker_done = "merged_or_closed"`, when a PR that closes it has merged into its default branch.
