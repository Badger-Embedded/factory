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
| Approval gates | Spec, plan and publish. Spec and plan are on by default. The settings page turns each on or off. A job copies the gates from the settings when it is created, so a later settings change does not touch existing jobs. Everything after the last gate runs without stopping, apart from questions the agent asks. The state machine stops at a gate only when the job has it. The settings page also has a publish gate, off by default. A job with it stops before the first push of each planned pull request and waits for approval. Later pushes to the same pull request, such as fixes for CI or review comments, are not gated. You merge on GitHub, unless the auto-merge setting is on: then the factory squash-merges a pull request once every check passed and no review comment waits for the agent. A pull request with no checks reported yet is not merged, so a push is never merged before GitHub starts its CI. |
| Notifications | The dashboard inbox only. |
| Job board | The index page groups the selected project's jobs in four lanes by who must act: You, Agent, Factory and Done. The lanes replace the earlier "Waiting for you" and "All jobs" lists. One function decides a job's lane and its turn badge from the same stage and turn rules, so they never disagree. A merged, failed or cancelled job is in Done. A job that needs an answer or approval, or a review on GitHub, is in You. A job in review with auto-merge on is in Factory. Each lane shows a count and, when empty, a message. Done is a collapsed `<details>` element, so it works without JavaScript. Lanes list the newest job first. Cards in You that wait at the spec or plan gate have an Approve button. It posts to `POST /jobs/{id}/approve` with the selected project, and the handler redirects back to that project's board. The job page's own approve form sends no project, so it still redirects to the job page. Cards waiting on questions, on the publish gate or on review have no button, and neither do cards in other lanes. Every other action stays on the job page. |
| Project registry | `projects/<name>/project.toml` files in this repository. Adding a project is a pull request. |
| Request images | The New request form posts multipart. The handler accepts up to 5 images of at most 5 MiB each, and the request text may be at most 256 KiB. It checks each image by its first bytes, not by file name or declared type, and accepts PNG, JPEG, GIF and WebP. A request over a limit, or one with images and no text, gets a 400 that names the limit and creates no job. The factory saves each image as `<data_dir>/jobs/<id>/images/image-<n>.<ext>` under a name it generates, so an upload's file name never reaches the file system. The job row in SQLite records only the file names, in a `job_images` table. `factory-core`'s `Job` holds no image data, since it does no I/O. A small script, `static/paste-images.js`, attaches an image pasted into the request textarea to the file input, shows a thumbnail with a Remove button, and leaves the form working without the script. The job page shows the images next to the request, served from `GET /jobs/<id>/images/<name>`, which serves only a name the `job_images` table lists for that job. When a run starts a fresh session, the runner copies the images into the checkout under `.git/factory-images/`, which git does not track, so the checkout stays clean. It appends their paths to the prompt and tells the agent to look at them. The clarify run is the first such run. A run that resumes a session adds nothing, because the session already has the paths. |
| Job expiry | The job loop deletes a merged, failed or cancelled job once its `created_at` is more than 30 days old. The age counts from the request, not from the day the job finished. The factory deletes the job row, its rows in the other tables, and `<data_dir>/jobs/<id>`. A job that is not finished stays however old it is. When one job fails to delete, the loop logs it and goes on with the others, and the next pass tries it again. |
| Capacity | At most 2 worker containers at a time. Each worker gets `--memory 3g --cpus 4`. A job waiting for you holds no worker. The limit, image, memory and CPUs are settings stored in SQLite, read by the job loop on every pass, so a settings page can change them without a restart. |
| Job cost | Each job has a running total of the API cost of its agent runs, in USD. It lives in the `job_costs` table in SQLite, one row per job, so a job from before the table existed has no row and reads 0. `Scheduler::agent` adds the cost a run reports, and a run that reports none adds nothing. A cancelled or failed job keeps its total. `format_cost` formats it as `$1.23`, for the log and for the pages later. No page shows it yet. |

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

Each job moves through these states. Each ✋ is an approval gate. A job has the gates its settings turned on when it was created.

```text
requested → clarifying → spec ✋ → plan ✋ → implementing → verifying → publish ✋ → pr-open → ci-and-review → merged
```

- **clarifying.** The agent reads the repository and asks questions. A headless run cannot wait for answers, so the run ends with its questions as structured output. The dashboard shows them, and the next run resumes the same session with your answers.
- **spec.** The agent writes the goal, scope, what is out of scope, and acceptance criteria.
- **plan.** The agent splits the work into pull requests. A large refactor gets its own pull request first, as maestro's `CONTRIBUTING.md` requires.
- **implementing.** One worker session per pull request. The worker commits in the job workspace and stops when the commits are ready.
- **verifying.** The factory runs the project's checks in the worker and the comment review. Failures go back to the session.
- **publish.** With the publish gate on, the job stays in verifying once the checks pass and the factory collects the branch's commits and diff stat from the checkout. The dashboard shows them with the pull request title and description. Approve moves the job to pr-open. Request changes sends your feedback to the pull request's session, then the checks run again and the job returns here. A revision does not count toward the 5 failed rounds. The factory pushes nothing before you approve.
- **pr-open.** The factory pushes the branch and opens the pull request as moonthebot.
- **ci-and-review.** The factory polls CI and review comments, and feeds failures and your comments back into a resumed session until CI is green and you merge. When the pull request no longer merges cleanly, the factory fetches the newest base branch into the checkout, since workers have no GitHub credentials, and the agent rebases onto it. A conflict counts toward the job's limit of 5 failed rounds per pull request, so an agent that cannot resolve one stops.

The state machine is pure logic with no I/O, so it can be tested without podman or GitHub. Every step must be safe to retry, because a container or the factory can die in the middle of one.

## Running Claude Code in a worker

- One `claude -p` run per stage, with `--output-format stream-json` for the live log. The factory picks the session's UUID and passes it with `--session-id` on the first run, so it knows the ID even if it dies during the run. Later runs pass `--resume <session-id>`.
- Every run ends with `--json-schema` structured output: an `outcome` and the questions, spec or plan that go with it. Each stage allows `questions` and one outcome of its own: `clarified`, `spec`, `plan` or `commits_ready`. Claude Code puts the output in `structured_output` on the final `result` line. A run without it, or with an outcome its stage does not allow, fails the job.
- The worker is started with `--rm`, so each job gets a Claude Code config directory on the host, mounted at `CLAUDE_CONFIG_DIR`. It keeps the session files that `--resume` reads.
- The token reaches the worker as `--secret claude-token,type=env,target=CLAUDE_CODE_OAUTH_TOKEN`.
- Every run gets `--settings` with `attribution` emptied. Claude Code otherwise adds a `Co-Authored-By` trailer to each commit, a skill's rules do not stop it, and GitHub copies the branch's co-author trailers into the squash commit on the base branch. Before the project's checks, the factory also fails a branch whose commits credit an AI tool, and the agent rewords them.
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
- **Overlap with maestro's skills.** Once the factory works, decide whether maestro keeps its own `no-comments` and `open-pr` skills for interactive sessions or drops them.

## Lessons from building maestro's checks

- `grep -q` at the end of a pipeline under `set -o pipefail` can fail when it matches, because the writer gets SIGPIPE. Feed `grep -q` from a variable instead.
- macOS has bash 3.2 as `/bin/bash`. Hook scripts must work there.
- prek refuses to run when `prek.toml` is not staged.
- prek runs hooks without a `stages` setting at every stage, the `commit-msg` stage included. Set `default_stages` when adding hooks for other stages.
- A project setting in `.claude/settings.json` loads only when the session starts. A hook added mid-session does not fire until the next session.
