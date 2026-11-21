# agents

The factory's own agent material, bundled into the worker image and loaded by
whichever harness runs a stage, Claude Code or Codex. The worker image copies
this directory to `/factory-agents`.

## Layout

```text
.claude-plugin/plugin.json   Claude's manifest; --plugin-dir needs it next to skills/
skills/review/SKILL.md       judges a branch's diff before a merge
skills/comment-review/SKILL.md  checks the comments a diff adds
schemas/review.json          the review skill's approve/changes output schema
```

## How each harness loads it

- **Claude Code.** When the factory is configured with the agents directory
  (`FACTORY_AGENTS_DIR`), it mounts that directory at `/factory-agents` and a
  reviewer run passes `--plugin-dir /factory-agents`, so the skills load under
  the `factory:` namespace from the plugin named in `.claude-plugin/plugin.json`.
  A reviewed repository's own skills load alongside them. With the variable
  unset, the image's baked-in copy is present but a run gets no `--plugin-dir`
  flag.
- **Codex.** The worker image links `/etc/codex/skills/factory` to
  `/factory-agents/skills`. Codex reads skills from `/etc/codex/skills`, so a
  review run finds `review` and `comment-review` there without the
  `--plugin-dir` flag. Codex has no plugin manifest, so it reads only `skills/`.

The skills name no single harness's rules file. Each reads the repository's
agent instructions, `AGENTS.md` and `CLAUDE.md` where it carries them, and both
when it has both.
