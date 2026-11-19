# factory

A dashboard where you ask for a feature, and Claude Code agents clarify it with you, plan it, implement it in an isolated container and open a pull request. It runs on `moon.local` with rootless podman.

See [docs/design.md](docs/design.md).

## Run the dashboard locally

```sh
nix develop -c cargo run -p factory-server
```

Then open <http://127.0.0.1:8080>. Without `tailscale serve` in front, the dashboard shows the user as `local`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `FACTORY_LISTEN` | `127.0.0.1:8080` | Address the dashboard listens on |
| `FACTORY_DATA` | `data` | Directory for the database and each job's checkout and Claude Code sessions |
| `FACTORY_DB` | `data/factory.db` | SQLite database file |
| `FACTORY_PROJECTS` | `projects` | Directory of `<name>/project.toml` files |
| `FACTORY_WORKERS` | unset | `podman` runs jobs in podman workers. Unset, jobs wait in `requested`. |
| `FACTORY_PODMAN` | `podman` | The podman binary. Set `CONTAINER_HOST` to use a remote podman service. |
| `FACTORY_PLUGIN_DIR` | unset | The factory plugin, mounted read-only into workers |

The worker limit, image, memory and CPUs are settings stored in the database. The job loop reads them on every pass.

Without `FACTORY_WORKERS=podman`, a new job waits in `requested`. To see the pages for the later stages, fill the database with demo jobs in every stage first:

```sh
nix develop -c cargo run -p factory-server --example seed
```
