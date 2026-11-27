# factory

A dashboard where you ask for a feature, and Claude Code agents clarify it with you, plan it, implement it in an isolated container and open a pull request. It runs on `moon.local` with rootless podman.

See [docs/design.md](docs/design.md).

## Run the dashboard locally

```sh
nix develop -c cargo run -p factory-server
```

Then open <http://127.0.0.1:8080>. Without `tailscale serve` in front, the dashboard shows the user as `local`.

Run the browser tests with `nix develop -c just e2e`. They start their own server and need no setup.

| Variable | Default | Meaning |
| --- | --- | --- |
| `FACTORY_LISTEN` | `127.0.0.1:8080` | Address the dashboard listens on |
| `FACTORY_DATA` | `data` | Directory for the database and each job's checkout and Claude Code sessions |
| `FACTORY_DB` | `data/factory.db` | SQLite database file |
| `FACTORY_IMPORT_PROJECTS` | `projects` | Directory of `<name>/project.toml` files to import once into the database. Read only on the first start of a database with no projects, then its files are deleted. Projects otherwise live in the database and are onboarded from the UI. |
| `FACTORY_WORKERS` | unset | `podman` runs jobs in podman workers. Unset, jobs wait in `requested`. |
| `FACTORY_PODMAN` | `podman` | The podman binary. Set `CONTAINER_HOST` to use a remote podman service. |
| `FACTORY_AGENTS_DIR` | unset | The factory's bundled agent material, mounted read-only into workers |
| `FACTORY_WORKER_VOLUMES` | unset | Space-separated `--volume` values for every worker, such as `factory-nix:/nix` |
| `GH_TOKEN` | unset | Read once at startup to give each existing GitHub project that token as its project token, when the secret store is available. Jobs and onboarding do not use it. Workers never get it. |
| `FACTORY_GH` | `gh` | The GitHub CLI |
| `FACTORY_GLAB` | `glab` | The GitLab CLI, used for Salsa projects |
| `FACTORY_MCP_CA_FILE` | unset | PEM file of extra root certificates that the MCP connection check and the MCP proxy trust next to the built-in roots, for a server whose certificate is not from a public CA. The factory stops at startup if the file cannot be read. |
| `FACTORY_FORGEJO_CA_FILE` | unset | Test only. PEM file of extra root certificates that the Forgejo version check and API calls trust next to the built-in roots. The browser tests use it for their stub server. A real Forgejo server needs a certificate from a public CA. The factory stops at startup if the file cannot be read. |

The worker limit, image, memory and CPUs are settings stored in the database. The job loop reads them on every pass.

Without `FACTORY_WORKERS=podman`, a new job waits in `requested`. To see the pages for the later stages, fill the database with demo jobs in every stage first:

```sh
nix develop -c cargo run -p factory-server --example seed
```

## Forge accounts

The factory has no account of its own. Each operation of a job runs as one of two tokens, both stored in the encrypted secret store:

- The project token, saved on the project's settings page in the Forge account card. Any collaborator can save or clear it. Jobs run as it unless the rule below applies. Renovate discovery, Renovate jobs and gap proposals before approval always run as it.
- A user token, saved on the forge's page under Forges in the settings sidebar. A request job runs as it when its requester has stored one and has turned on "Use my account" for the project, a switch on the same card that is off by default.

A project with no project token cannot run jobs that use it, and the board and the project page say so. A token the forge refuses fails the operation with a message that names the token and the page that replaces it. The account of a token should be a bot account: the factory ignores pull request comments from it, and GitHub does not let an account approve its own pull request. A job's commits carry the account's login and its noreply address.

A packaging project can read its upstream repository on another forge than its own. The Maintenance card has an "Upstream forge" select and a "Check connection" button that shows which credential was used and whether the repository was readable. A job reads the upstream with the optional upstream token when one is stored, with the project token when the upstream is on the project's forge, and with no token otherwise. A public GitHub upstream needs no token, but unauthenticated reads are rate limited.

The access each account needs on the repository:

| Use | Access |
| --- | --- |
| Request jobs: clone, push, open a pull request | Write |
| Renovate triage: list pull requests, read status, logs and diffs, comment | Read |
| Renovate triage: rerun failed CI, approve and merge | Write |

With only Read, Renovate triage still works: it skips the reruns and goes straight to the failure analysis, comments, and leaves a safe pull request as ready to merge even with auto-merge on. Granting Write takes effect on the next head commit, with no restart.

## Forgejo servers

A factory admin registers each Forgejo server, such as a self-hosted one or Codeberg, on Forgejo servers in the Factory group of the settings sidebar (`/settings/forgejo`). Enter a name of 1 to 32 characters of `a-z`, `0-9` and `-`, and the server's `https` URL, with its port or sub-path if it has one. The factory reads `/api/v1/version` and refuses a server that does not answer as Forgejo. A version below 17 cannot rerun failed CI, and a version below 16 cannot serve failed job logs. The page warns about each. A name and a URL cannot be edited, because stored tokens belong to the name. Delete the server and add it again to change them. Delete is refused while a project uses the server, and it removes every token stored for it.

Each login saves a personal access token for a server on its page in the Forges group, `/settings/forges/<name>`. The token needs the scopes `read:user`, `write:repository` and `write:issue`. A project on a registered server can be onboarded from the wizard, where each server is a Forge option and the repository is `owner/name`, and can run request jobs, Renovate jobs and packaging. Forgejo has no fixed Renovate account, so set the Renovate account in the project's Maintenance card before you turn Renovate on. A packaging project can name a Forgejo server as its upstream forge.

Forgejo tells only a repository or instance admin what access another user has. To take pull request comments from other people, the token must belong to an admin of the repository. With a Write-only token the factory cannot check a commenter, so it ignores their comments.

## Deploy to moon

```sh
just deploy            # origin/main
just deploy my-branch  # any git ref
```

`deploy/deploy.sh` copies the ref's source to `~/factory-src` on `moon.local` and runs `deploy/install.sh` there. That script builds the worker and factory images, installs the Quadlet unit `deploy/factory.container`, restarts `factory.service` and publishes the dashboard on the tailnet with `tailscale serve`. It creates no `gh-token` podman secret. When one exists from an earlier install, the unit passes it in so the factory can copy it into each project's token once, and you can remove the secret afterwards. The factory holds the Claude token in its secret store and you set it on the settings page, so no podman secret is required; when a `claude-token` secret still exists, the unit passes it in so the factory can import it once.

On moon, the factory keeps its database and job directories in `~/.local/share/factory`. Follow its log with `journalctl --user -u factory -f`.
