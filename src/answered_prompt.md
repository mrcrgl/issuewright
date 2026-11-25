issuewright resumed this session. Earlier you asked a question here that nobody answered in time, so you moved it to {repo}#{number} and stopped. The issue has since been released again with `dispatch:ready`, so the answer should be on GitHub.

You are in `{dir}` on branch `{branch}`, at what you pushed before stopping.{moved_note} Changes you didn't push are gone; redo them if you still need them.

1. Read the comments since your question: `gh issue view {number} --repo {repo} --json comments --jq '.comments[-5:]'`.
2. Carry on from where you stopped, following the instructions you started with. They still apply, including how to hand off once the work is done.
3. If the comments don't answer your question, ask again here. If nobody answers in time, issuewright tells you to move it to GitHub again.
