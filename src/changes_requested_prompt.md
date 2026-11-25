issuewright resumed this session: the reviewer requested changes on {pr} at `{head_sha}`. This is review round {round} of at most {max_rounds}; after that, the PR goes to a person.

You are in `{dir}` on branch `{branch}`, at the head you pushed.{moved_note}

1. Read the review and what the reviewer wrote on the issue: `gh pr view {pr_number} --repo {repo} --json reviews,comments` (the latest review requesting changes), its inline comments with `gh api repos/{repo}/pulls/{pr_number}/comments`, and `gh issue view {number} --repo {repo} --comments`.
2. Fix every finding that blocks, the way the issue and the repository's rules ask for it. Fix the nits too when they are cheap and in scope. Don't do more than the issue asks for.
3. Build and run the tests your changes affect. Commit, and push with a plain `git push`, as a command of its own.
4. Comment on the issue, finding by finding, what you changed (with the commit), and for anything you deliberately left as it is, why. The next reviewer reads that comment. Then hand the PR back to CI: `gh issue edit {number} --repo {repo} --add-label dispatch:ci --remove-label dispatch:running`, and stop. Once CI is green, a fresh review follows.

If a finding is wrong, explain why in that comment instead of changing the code; the next reviewer decides.

If a finding needs a decision the issue doesn't make (scope, a trade-off, anything against production), don't guess. Fix what you can, push it, and comment the open decision on the issue. Then, this session runs in {mode} mode:

- interactive: ask the user here, and wait for the answer. If nobody answers in time, issuewright tells you to move the question to GitHub.
- unattended: set `dispatch:needs-input` (removing `dispatch:running`), and stop.
