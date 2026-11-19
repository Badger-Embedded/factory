# Factory design

The factory turns feature requests into pull requests. You describe a change in a dashboard. Claude Code agents clarify it with you, write a spec and a plan for you to approve, implement it in an isolated container, and open a pull request. You review and merge on GitHub.

The factory is separate from the projects it works on. Its skills, agents, hooks and per-project configuration live in this repository and are never committed to a target repository. A target repository keeps only its own rules, which bind people too: `CLAUDE.md`, `CONTRIBUTING.md`, CI and its GitHub ruleset. [maestro](https://github.com/Badger-Embedded/maestro) is the first project.

These decisions were made on 2026-11-19 while brainstorming in the maestro repository.

## Decisions

| Topic | Decision |
| --- | --- |
| Agent runtime | The Claude Code CLI in rootless podman worker containers. Claude Managed Agents was considered and rejected in favor of local Claude Code on our own hardware. |
| Host | `moon.local` (`ssh moon@moon.local`): Ubuntu 26.04, x86_64, 8 cores, 7.2 GB RAM, 74 GB free disk, systemd linger on for user `moon`. Tailscale is up at `100.77.68.75`. |
| Factory process | A Rust binary in its own container, run as a Quadlet systemd user service. It starts worker containers through the user's `podman.socket`. |
| Factory stack | axum, SQLite, server-rendered pages with htmx, and server-sent events for live logs. No frontend build step. |
| Dashboard access | The factory listens on localhost. `tailscale serve` publishes it over HTTPS on the tailnet and passes the Tailscale login in request headers. No separate login. |
| GitHub identity | The bot account `moonthebot`, logged in with `gh` on moon (scope `repo`). It must be a **Write** collaborator on each target repository, not Admin, so a ruleset's Admin bypass does not apply to it. |
| Credentials | Only the factory holds the GitHub token. Workers get no GitHub credentials. |
| Claude auth | The subscription token from `claude setup-token`, stored as a podman secret and given only to workers. Its usage limits are shared with interactive use. Switch to an API key if the factory hits them often. |
| Approval gates | Spec and plan. Everything after the plan runs without stopping, apart from questions the agent asks. You merge on GitHub. |
| Notifications | The dashboard inbox only. |
| Project registry | `projects/<name>/project.toml` files in this repository. Adding a project is a pull request. |
| Capacity | At most 2 worker containers at a time. Each worker gets `--memory 3g --cpus 4`. A job waiting for you holds no worker. The limit, image, memory and CPUs are settings stored in SQLite, read by the job loop on every pass, so a settings page can change them without a restart. |

## Architecture

```text
moon.local (rootless podman, user moon)
├─ factory container (Quadlet user service)
│    orchestrator + dashboard + SQLite
│    holds moonthebot's gh token
│    talks to podman through podman.socket
│    polls GitHub for CI status and review comments
└─ worker containers, one per job stage, started by the factory
     image: Claude Code, git, nix
     mounts: job workspace, shared /nix store volume, cargo cache per project, factory plugin (read-only)
     env: Claude token only
```

GitHub cannot reach `moon.local`, so the factory polls GitHub with `gh api` about once a minute instead of receiving webhooks.

## Job flow

Each job moves through these states. Each ✋ is an approval gate.

```text
requested → clarifying → spec ✋ → plan ✋ → implementing → verifying → pr-open → ci-and-review → merged
```

- **clarifying.** The agent reads the repository and asks questions. A headless run cannot wait for answers, so the run ends with its questions as structured output. The dashboard shows them, and the next run resumes the same session with your answers.
- **spec.** The agent writes the goal, scope, what is out of scope, and acceptance criteria.
- **plan.** The agent splits the work into pull requests. A large refactor gets its own pull request first, as maestro's `CONTRIBUTING.md` requires.
- **implementing.** One worker session per pull request. The worker commits in the job workspace and stops when the commits are ready.
- **verifying.** The factory runs the project's checks in the worker and the comment review. Failures go back to the session.
- **pr-open.** The factory pushes the branch and opens the pull request as moonthebot.
- **ci-and-review.** The factory polls CI and review comments, and feeds failures and your comments back into a resumed session until CI is green and you merge.

The state machine is pure logic with no I/O, so it can be tested without podman or GitHub. Every step must be safe to retry, because a container or the factory can die in the middle of one.

## Running Claude Code in a worker

- One `claude -p` run per stage, with `--output-format stream-json` for the live log. The factory picks the session's UUID and passes it with `--session-id` on the first run, so it knows the ID even if it dies during the run. Later runs pass `--resume <session-id>`.
- Every run ends with `--json-schema` structured output: an `outcome` and the questions, spec or plan that go with it. Each stage allows `questions` and one outcome of its own: `clarified`, `spec`, `plan` or `commits_ready`. Claude Code puts the output in `structured_output` on the final `result` line. A run without it, or with an outcome its stage does not allow, fails the job.
- The worker is started with `--rm`, so each job gets a Claude Code config directory on the host, mounted at `CLAUDE_CONFIG_DIR`. It keeps the session files that `--resume` reads.
- The token reaches the worker as `--secret claude-token,type=env,target=CLAUDE_CODE_OAUTH_TOKEN`.
- The factory's rules come in through `--plugin-dir` (skills and agents) and `--settings` (hooks). A headless run in an untrusted folder ignores a repository's `extraKnownMarketplaces` without any message, so the factory must not depend on the target repository's plugin settings.
- The target repository's own `CLAUDE.md` and project skills still load. maestro has its own `no-comments` and `open-pr` skills, so the factory's plugin namespaces its versions as `factory:no-comments` and `factory:open-pr`.
- Claude Code hooks run under whatever shell the container has, without the nix devShell. Write hook scripts for plain bash and test them that way.

## Layout

```text
crates/
  factory-core/      job state machine, pure, no I/O
  factory-server/    axum dashboard, SQLite, job loop, settings
  factory-runner/    podman workers, stream-json, git checkouts, gh
containers/
  factory/Containerfile
  worker/Containerfile
deploy/              Quadlet unit, deploy.sh and install.sh
projects/            factory and maestro
```

Not built yet: `plugin/` with the factory skills and agents (unslop, no-comments and comment-reviewer, spec interview, plan and split, open-pr), the comment review in `verifying`, and the live log over server-sent events. The job page reloads every 5 seconds while the agent works. A small inline script, `static/log-scroll.js`, keeps the log position across each reload: it saves in `sessionStorage` whether the reader was at the bottom or how far up they had scrolled, and restores that after the reload. Server-sent events will replace the reload and the script.

`open-pr` changes from maestro's version: the worker stops when its commits are ready, and the factory pushes and opens the pull request.

This repository follows the same standards as maestro: `CONTRIBUTING.md`, a check on pull request titles and descriptions, and prek hooks. It has no ruleset on `main`. GitHub allows rulesets on a private repository only on a paid plan, and the factory stays private for now. Nothing on GitHub blocks a direct push or a merge with failing checks, so the reviewer merges only after CI passes. Merges are squash only, and GitHub deletes the branch after a merge.

## First milestone

One maestro job from start to end: a request typed in the dashboard ends as a green pull request from moonthebot.

## Prerequisites

| Step | Who | Status on 2026-11-20 |
| --- | --- | --- |
| `sudo apt install podman` on moon (5.7 is in the archive) | You, needs sudo | Done |
| `systemctl --user enable --now podman.socket` on moon | You | Done |
| `claude setup-token`, then `podman secret create claude-token -` on moon | You | Done |
| Add `moonthebot` to maestro as a Write collaborator | You | Not done. moonthebot has Read. |
| Create `Badger-Embedded/factory`, private | Claude | Done |
| Add `moonthebot` to factory as a Write collaborator, so the factory can work on itself | You | Done |
| `just deploy` | Claude | Done. The dashboard is on `https://moon.rex-mimosa.ts.net`. |

## Open questions

- **Sign-off on factory pull requests.** maestro's `CONTRIBUTING.md` asks for a `Signed-off-by` line in the pull request description, and its `open-pr` skill signs with the git identity. A pull request from moonthebot needs a decision: no sign-off, or yours added when you approve the plan.
- **Configurable approval gates.** The gates are fixed in `factory-core` today: spec and plan. To let a settings page add gates, such as approving before the factory pushes, a job would carry its set of gates, copied from the settings when the job is created. A job then keeps the gates it started with when the settings change. The state machine stops at a gate only when the job's set includes it.
- **Overlap with maestro's skills.** Once the factory works, decide whether maestro keeps its own `no-comments` and `open-pr` skills for interactive sessions or drops them.

## Lessons from building maestro's checks

- `grep -q` at the end of a pipeline under `set -o pipefail` can fail when it matches, because the writer gets SIGPIPE. Feed `grep -q` from a variable instead.
- macOS has bash 3.2 as `/bin/bash`. Hook scripts must work there.
- prek refuses to run when `prek.toml` is not staged.
- prek runs hooks without a `stages` setting at every stage, the `commit-msg` stage included. Set `default_stages` when adding hooks for other stages.
- A project setting in `.claude/settings.json` loads only when the session starts. A hook added mid-session does not fire until the next session.
