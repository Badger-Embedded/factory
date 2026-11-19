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
| `FACTORY_DB` | `factory.db` | SQLite database file |
| `FACTORY_PROJECTS` | `projects` | Directory of `<name>/project.toml` files |

No worker runs yet, so a new job waits in `requested`.
