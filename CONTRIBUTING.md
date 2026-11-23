# Contributing to factory

## Setup

Enter the dev shell with `nix develop`, then run `just hooks`. It installs a `pre-commit` hook that runs the linters on staged files. Run `just lint` before you push.

## One pull request per change

Pull requests land on `main` by squash merge. Each pull request becomes one commit, so a pull request holds one feature or one fix. Anyone can then cherry-pick that commit to another branch and get the whole change.

The pull request carries everything the change needs:

- **Code.**
- **Tests.** See [Tests](#tests). `tests-accompany-changes` fails a pull request that changes `crates/*/src/` without touching any `crates/*/tests/`.
- **Docs.** Update the docs that the change makes wrong: READMEs, `docs/design.md`, and `AGENTS.md`, `.agents/` or `.claude/` when the change affects how agents work. No check enforces this. Review does.

Keep unrelated changes out. A fix found while building a feature gets its own pull request.

If a change needs a large refactor first, open the refactor as its own pull request and land it first. Its commits carry a `No-Tests: <reason>` trailer, such as `No-Tests: pure rename, covered by the job flow tests`. Then rebase the feature branch onto `main` and open the feature pull request. After the refactor is squashed, its original commits are still on your feature branch, so drop them while you rebase:

```sh
git fetch origin
git rebase --onto origin/main refactor-branch feature-branch
```

## Pull request title

The title becomes the subject of the commit on `main`. It uses the kernel style:

```text
area: summary
area: subarea: summary
```

- The area is a directory or file name, without its extension, on the path of a file the pull request changes. For a change under `crates/factory-core/src/`, `factory-core` and `crates` both work. Pick the most specific one.
- Three areas do not match a path. `ci` is for `.github/`, `nix` is for `flake.nix` and `flake.lock`, and `treewide` is for a change across more than one top-level directory.
- Areas are lowercase. A directory with a leading dot drops the dot, so `.claude/` is `claude`.
- The summary is imperative and lowercase, with no period at the end: "retry on EBUSY", not "Retried on EBUSY.".
- The title has at most 64 characters, so it still fits in 72 when GitHub appends ` (#123)`.

Examples:

```text
factory-core: job: retry a worker that died mid-run
worker: add nix to the image
lint: reject tests inside src/
```

## Pull request description

The description becomes the body of the commit on `main`. Someone reading `git log` or `git blame` in two years has only this text to learn why the change exists. The diff shows what changed, so spend the words on why.

Use these sections. The template has them already.

```markdown
## Why

The problem, how it showed up, and why this solution over the others
you considered.

## Docs

The docs this pull request updates, or `None: <reason>`.

## Verification

What you ran and what it showed.
```

- Delete HTML comments. They would end up in the commit message.
- End with trailers, one per line:
  - `Fixes: <sha> ("<subject>")` names the commit that introduced the bug. Use at least 12 characters of the sha. Whoever cherry-picks the fix can tell from it which branches have the bug.
  - `Closes: #<issue>` links an issue, and GitHub closes it when the pull request merges.
  - `Signed-off-by: Name <email>` for each author, as described in [Commits inside a pull request](#commits-inside-a-pull-request).

A trivial change may keep the sections short, but it still needs them. Examples of trivial changes: a typo, formatting, or a dependency bump with no code change. In that case "Why" can be one line.

## Tests

Test a crate through its public API, in the crate's `tests/` directory. The `no-inline-tests` lint rejects tests in `src/`.

`factory-core` does no I/O, so its tests need no podman, GitHub or database. The `core-no-io` lint rejects file, network, process, environment, thread and clock access in it.

The browser tests in `e2e/` drive the real `factory-server` binary in headless Chromium with Playwright. They cover the dashboard's pages and the scripts in `crates/factory-server/static/`, which no Rust test loads in a browser. Run them with `nix develop -c just e2e`. The nix dev shell provides the browser, so run no `playwright install`. To run one test, pass Playwright's arguments: `just e2e tests/board.spec.ts -g "new request"`. The suite starts the server on free ports with a temporary database, and the fixture program `crates/factory-server/examples/e2e_fixture.rs` seeds it. The project MCP tests start a stub MCP server over TLS from `e2e/mcp-stub.ts`. A change to a page's behaviour in the browser, or to a file in `crates/factory-server/static/`, adds or updates a browser test. `just test` runs the Rust tests only and needs no browser.

A commit that changes no behaviour, such as a rename, carries a trailer with the reason instead of a test, for example `No-Tests: pure rename, covered by the job flow tests`. The trailer exempts only that commit's own changes. Run `just check-tests` before you push.

Hook scripts run in worker containers under plain bash, without the nix dev shell, and on macOS under bash 3.2. Test them that way.

## Commits inside a pull request

The squash merge discards your branch's commit messages, so write them for the reviewer.

End the pull request description with a `Signed-off-by: Name <email>` line for each author. It certifies the [Developer Certificate of Origin](https://developercertificate.org/), which says you have the right to submit the code. The description becomes the commit on `main`, so the line lands there. Signing off your branch commits with `git commit -s` does not count, because the squash merge discards them. No check enforces this yet. That will change once the project has a license and takes outside contributions.

Do not credit AI tools in commits or pull requests. An agent commits as the person who runs it, and that person answers for the change.

## What checks these rules

| Rule                                                              | Checked by                                                           |
| ----------------------------------------------------------------- | -------------------------------------------------------------------- |
| Title format, area and length                                     | `check-pr-message.sh` in the PR workflow, or `just check-pr` locally |
| Why, Docs and Verification sections, trailer format, no AI credit | `check-pr-message.sh`                                                |
| Code changes come with tests                                      | `tests-accompany-changes` (`just check-tests`)                       |
| Tests live in `tests/`, `factory-core` does no I/O                | `no-inline-tests` and `core-no-io` ast-grep rules                    |
| Docs updated with the change                                      | Review                                                               |
| `Signed-off-by` in the description                                | Review                                                               |

## Agents

Coding agents follow the same rules, and the checks above apply to agents and people alike.
