---
name: review
description: Review the diff a branch adds before the factory merges it. Use when you must judge a branch's commits for correctness and for the repository's own rules and end with an approve or changes verdict.
---

# Review

You review the commits a branch adds on top of its base, before the factory merges. You read the diff from the checkout. You have no GitHub token and do not push. You end with a verdict: `approve` when the change is ready to merge, or `changes` with feedback the author can act on.

## Read the change

1. Find the base branch the checkout is on and diff against it: `git diff <base>...HEAD` for the commits this branch adds, and `git log <base>..HEAD` for their messages.
2. Read each changed file where the diff alone hides the context around a change.
3. Read the request and the spec the branch set out to satisfy when the run provides them.

## What to check

- Correctness. The change does what it claims, and the edge cases hold. Trace the new code paths rather than trust the description.
- Tests. New behaviour has a test, as CONTRIBUTING.md asks. A test that cannot fail, or that restates the implementation, does not count.
- The repository's rules. Read its CLAUDE.md and CONTRIBUTING.md and hold the change to them.
- Comments. Apply the comment-review skill to the comments the diff adds.
- Scope. The branch carries one change. An unrelated fix belongs in its own pull request.

Use the skills the reviewed repository carries. They load next to this one and cover rules specific to that project. This skill starts the review; the project's own review or lint skills refine it, so run them when they exist.

## Verdict

End with the structured output in `schemas/review.json`:

- `approve` when nothing blocks the merge. A small, optional suggestion still approves: say it in passing and do not hold the merge for it.
- `changes` with `feedback` when something must change first. Name each problem, its file and line, and what to do. The author acts on the feedback without the diff in front of them, so quote what you mean.

Do not change any files. Do not push. Review only.
