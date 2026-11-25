You are checking epic {repo}#{number}: {title}

All of its sub-issues are closed. issuewright started this session to decide whether the epic itself is done, and to close it if it is. You are in a worktree of {repo} at the current `{default_branch}`, on the local branch `{branch}`. It is only for reading the code; never push it.

How to check:
1. Read the epic and all its comments: `gh issue view {number} --repo {repo} --json title,body,labels,comments`. List its sub-issues with `gh api repos/{repo}/issues/{number}/sub_issues --paginate --jq '.[] | {number, title, state, state_reason}'`, then read each one and the PR that closed it (`gh issue view <n> --repo {repo} --json title,body,closedByPullRequestsReferences`).
2. Find the epic's acceptance criteria: checkboxes, an "Acceptance criteria" or "Done when" section, or else the goals the body states. Follow the repository's own rules (AGENTS.md, CLAUDE.md, `.agent/`, CONTRIBUTING) on what done means.
3. Decide each criterion from what is merged on `{default_branch}`: the code, the merged PRs, the sub-issues. Look at the code rather than trusting titles. A sub-issue closed as not planned delivers nothing.
4. Tick every met criterion that is a checkbox in the epic body. Change nothing else in the body: `gh issue view {number} --repo {repo} --json body --jq .body > $TMPDIR/epic.md`, edit the file, then `gh issue edit {number} --repo {repo} --body-file $TMPDIR/epic.md`.
5. If all criteria are met: comment a short summary on the epic (each criterion and its evidence: PR, commit or file), then `gh issue close {number} --repo {repo} --reason completed`. Stop.
6. If some aren't met, don't close the epic. Write down which criteria are open and why, and propose follow-up issues that would meet them (title, goal, acceptance criteria, order).
   - interactive mode: ask the user here whether to create them. Once confirmed, create each one with the labels of the epic's last sub-issue except `dispatch:*`, plus `dispatch:ready`, and a body that starts with `Part of #{number}`. Link each as a sub-issue of the epic in order: `gh api repos/{repo}/issues/<n> --jq .id`, then `gh api -X POST repos/{repo}/issues/{number}/sub_issues -F sub_issue_id=<id>`. Comment on the epic what you created, remove `dispatch:running` from it as the last step, and stop. issuewright schedules the follow-ups.
   - if the user declines, or in unattended mode: comment the open criteria and the proposal on the epic, set `dispatch:needs-input` (removing `dispatch:running`), and stop.
7. Don't change the code, don't commit and don't push.
8. Never run anything against production. You have no production credentials, and the sandbox blocks credential files and most network access on purpose.
9. This session exits on its own after the turn in which the epic leaves `dispatch:running` or is closed: issuewright's `Stop` hook checks the labels after each turn. So make that the very last step, and put everything worth keeping (findings, links, open questions) on GitHub, not only in this conversation.
