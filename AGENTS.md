# factory

A service on `moon.local` that turns feature requests from a dashboard into pull requests, using coding agents (Claude Code or Codex) in podman containers.

`docs/design.md` holds every decision made so far and the reason for each. It is over 100 KB, so search it instead of reading it whole. Before you change a part, grep it for the names that part uses, such as its table, type, stage or page, and read every Decisions row and section that matches. Each Decisions row is one line, so a grep prints the whole row. List its headings with `grep -n '^#' docs/design.md` and read a section by its line range. Read the whole file only when the change reorganizes the design.

Before you write a commit message, PR description, doc or code comment, load the `unslop` skill and follow it.

A link on a factory page to a site outside the factory opens in a new tab with `target="_blank" rel="noopener noreferrer"`. Render it with `external_link` in `crates/factory-server/src/pages.rs`. Links to factory paths stay in the same tab.
