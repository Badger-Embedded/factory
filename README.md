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
| `GH_TOKEN` | unset | GitHub token the factory clones, pushes and opens pull requests with until jobs move to project tokens. Onboarding checks no longer use it: they run as the onboarding login's own token. At startup it also seeds the project token of each existing GitHub project once, when the secret store is available. Workers never get it. |
| `FACTORY_GH` | `gh` | The GitHub CLI |
| `FACTORY_MCP_CA_FILE` | unset | PEM file of extra root certificates that the MCP connection check and the MCP proxy trust next to the built-in roots, for a server whose certificate is not from a public CA. The factory stops at startup if the file cannot be read. |
| `FACTORY_GIT_NAME`, `FACTORY_GIT_EMAIL` | `moonthebot` and its noreply address | Identity on the agent's commits |

The worker limit, image, memory and CPUs are settings stored in the database. The job loop reads them on every pass.

Without `FACTORY_WORKERS=podman`, a new job waits in `requested`. To see the pages for the later stages, fill the database with demo jobs in every stage first:

```sh
nix develop -c cargo run -p factory-server --example seed
```

## Deploy to moon

```sh
just deploy            # origin/main
just deploy my-branch  # any git ref
```

`deploy/deploy.sh` copies the ref's source to `~/factory-src` on `moon.local` and runs `deploy/install.sh` there. That script builds the worker and factory images, installs the Quadlet unit `deploy/factory.container`, restarts `factory.service` and publishes the dashboard on the tailnet with `tailscale serve`. It creates the `gh-token` podman secret from moonthebot's `gh` login when the secret is missing. The factory holds the Claude token in its secret store and you set it on the settings page, so no podman secret is required; when a `claude-token` secret still exists, the unit passes it in so the factory can import it once.

On moon, the factory keeps its database and job directories in `~/.local/share/factory`. Follow its log with `journalctl --user -u factory -f`.
