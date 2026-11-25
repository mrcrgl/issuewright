issuewright resumed this session: CI failed on {pr} at `{head_sha}`.

Failed checks:
{failed}

You are in `{dir}` on branch `{branch}`, at the head you pushed.{moved_note}

1. Read why each check failed: `gh pr checks {pr_number} --repo {repo}`, then `gh run view <run-id> --repo {repo} --log-failed`.
2. Fix the cause, commit, and push with a plain `git push`, as a command of its own. Never make CI green by skipping, disabling or weakening tests or checks.
3. Hand the PR back to CI as before: comment on the issue what you fixed, then `gh issue edit {number} --repo {repo} --add-label dispatch:ci --remove-label dispatch:running`, and stop. issuewright resumes you again if CI fails once more.

If the failure isn't caused by your change and a re-run can clear it (a runner that crashed or ran out of disk, a flaky test, a network timeout), re-run the failed jobs with issuewright's `session rerun-ci <run-id>` command (your system prompt names it). Comment on the issue which check failed, why it isn't your change, and that you re-ran it. Then hand the PR back to CI as in step 3, and stop.

If this was the third red round, or the failure isn't caused by your change and a re-run won't clear it (it fails on the default branch too, or `rerun-ci` refused), don't fix it. Comment the failing check and what you found on the issue. Then, this session runs in {mode} mode:

- interactive: tell the user here what failed and what you need, and wait for the answer. They may answer from the terminal or the Claude app, or run commands here themselves. If nobody answers in time, issuewright tells you to move the question to GitHub.
- unattended: set `dispatch:needs-input` (removing `dispatch:running`), and stop.
