//! The worker image carries a pinned Codex CLI and the factory review skills,
//! next to Claude Code. These tests read the Containerfile and the captured
//! `codex exec --json` fixture, so they need no podman and no login.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits under crates/<name>")
        .to_path_buf()
}

fn containerfile() -> String {
    fs::read_to_string(repo_root().join("containers/worker/Containerfile")).unwrap()
}

#[test]
fn the_worker_image_pins_a_codex_release() {
    let text = containerfile();
    assert!(
        text.contains("ARG CODEX_VERSION="),
        "the Containerfile does not pin a Codex version"
    );
    assert!(
        text.contains("github.com/openai/codex/releases/download/rust-v"),
        "Codex is not installed from an openai/codex release"
    );
    assert!(
        text.contains("codex-x86_64-unknown-linux-musl"),
        "the Containerfile does not fetch the x86_64 Linux release asset"
    );
}

#[test]
fn the_build_checks_the_installed_codex_version() {
    assert!(
        containerfile().contains("codex --version"),
        "the build does not run codex --version"
    );
}

#[test]
fn the_image_creates_a_codex_home_owned_by_the_worker() {
    let text = containerfile();
    assert!(text.contains("/codex"), "the image has no /codex directory");
    let creates_owned = text
        .lines()
        .any(|line| line.contains("chown worker:worker") && line.contains("/codex"));
    assert!(creates_owned, "/codex is not owned by the worker user");
}

#[test]
fn the_image_links_the_factory_skills_for_codex() {
    let text = containerfile();
    assert!(
        text.contains("/etc/codex/skills/factory"),
        "the image does not install the factory skills under /etc/codex/skills"
    );
    assert!(
        text.contains("ln -s /factory-plugin/skills /etc/codex/skills/factory"),
        "the factory skills are not linked from the bundled plugin"
    );
}

#[test]
fn the_image_still_installs_claude() {
    assert!(
        containerfile().contains("claude --version"),
        "the Codex change dropped the Claude install"
    );
}

#[test]
fn the_captured_run_is_valid_jsonl_with_the_documented_envelopes() {
    let path =
        repo_root().join("crates/factory-runner/tests/fixtures/codex-exec-auth-failure.jsonl");
    let text = fs::read_to_string(path).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();

    let first = &lines[0];
    assert_eq!(first["type"], "thread.started");
    assert!(
        first["thread_id"].as_str().is_some(),
        "thread.started carries the thread id the factory resumes with"
    );

    let types: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();
    for expected in ["turn.started", "error", "item.completed", "turn.failed"] {
        assert!(
            types.contains(&expected),
            "the fixture is missing a {expected} line"
        );
    }

    let last = lines.last().unwrap();
    assert_eq!(last["type"], "turn.failed");
    assert!(
        last["error"]["message"].as_str().is_some(),
        "turn.failed names the reason under error.message"
    );
}
