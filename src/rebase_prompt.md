issuewright resumed this session: {situation}

You are in `{dir}` on branch `{branch}`, at the head you pushed.{moved_note}

1. Rebase onto the current default branch with issuewright's `session rebase` command (your system prompt names it). It fetches and runs `git rebase origin/{default_branch}` outside the sandbox, which a plain `git rebase` needs once `{default_branch}` changed files the sandbox protects, such as `.claude/skills/`. At a conflict it stops: resolve, `git add` the files, then `session continue`.
2. Resolve each conflict so that both sides survive: what landed on `{default_branch}` since this branch was made, and this PR's change. Read the commits that touched a conflicting file (`git log origin/{default_branch} -- <file>`) to know what they meant. Never drop either side's work just to make a conflict go away.
3. Build and test what the resolution touched.
4. Push with `git push --force-with-lease`, as a command of its own. The rebase rewrote the branch, so a plain push is refused.
5. Comment on the issue what conflicted and how you resolved it. Then hand the PR back to CI: `gh issue edit {number} --repo {repo} --add-label dispatch:ci --remove-label dispatch:running`, and stop. Once CI is green, the rebased PR gets a review.

If the two sides contradict each other and resolving them needs a decision the issue doesn't make (for example, `{default_branch}` changed or removed what this PR builds on), don't guess: abort the rebase with `session abort` and comment the conflict and the decision it needs on the issue. Then, this session runs in {mode} mode:

- interactive: ask the user here, and wait for the answer. If nobody answers in time, issuewright tells you to move the question to GitHub.
- unattended: set `dispatch:needs-input` (removing `dispatch:running`), and stop.
