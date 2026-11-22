use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits under crates/<name>")
        .to_path_buf()
}

#[test]
fn claude_md_imports_agents_md() {
    let root = repo_root();
    assert!(
        root.join("AGENTS.md").is_file(),
        "AGENTS.md must hold the project instructions at the repository root"
    );
    let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert_eq!(
        claude.trim(),
        "@AGENTS.md",
        "CLAUDE.md must import AGENTS.md so Claude Code reads the one instructions file"
    );
}

#[test]
fn agents_md_tells_the_agent_to_load_unslop() {
    let agents = fs::read_to_string(repo_root().join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("unslop"),
        "AGENTS.md must tell the agent to load the unslop skill"
    );
}

#[test]
fn both_unslop_paths_resolve_to_one_file() {
    let root = repo_root();
    let scanned_by_codex = root.join(".agents/skills/unslop/SKILL.md");
    let scanned_by_claude = root.join(".claude/skills/unslop/SKILL.md");
    assert!(
        scanned_by_codex.is_file(),
        ".agents/skills/unslop/SKILL.md must exist, the path Codex scans"
    );
    assert_eq!(
        fs::canonicalize(&scanned_by_claude).unwrap(),
        fs::canonicalize(&scanned_by_codex).unwrap(),
        ".claude/skills/unslop must be a symlink that resolves to .agents/skills/unslop, so both \
         agents load the one skill"
    );
}
