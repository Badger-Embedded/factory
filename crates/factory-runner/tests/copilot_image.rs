//! The worker image carries a pinned GitHub Copilot CLI next to Claude Code and
//! Codex. Most of these tests read the Containerfile and the captured `copilot
//! -p --output-format json` fixtures, so they need no podman and no login; the
//! authenticated success fixture and the device-login storage facts came from
//! an operator capture, see "Copilot CLI facts" in docs/design.md.
//!
//! `the_worker_image_builds_and_runs_copilot_as_the_worker` is the exception:
//! it builds the image and runs the CLI, so it is `#[ignore]`d and run on
//! demand (`cargo test -p factory-runner --test copilot_image -- --ignored`),
//! where podman and the network the build downloads from are available. The
//! text checks guard the Containerfile cheaply on every run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
fn the_worker_image_pins_a_copilot_release() {
    let text = containerfile();
    assert!(
        text.contains("ARG COPILOT_VERSION="),
        "the Containerfile does not pin a Copilot version"
    );
    assert!(
        text.contains("npm install -g \"@github/copilot@${COPILOT_VERSION}\""),
        "Copilot is not installed from the pinned @github/copilot npm package"
    );
}

#[test]
fn the_build_checks_the_installed_copilot_version() {
    let text = containerfile();
    assert!(
        text.contains("copilot --version"),
        "the build does not run copilot --version"
    );
    assert!(
        text.contains("grep -qw \"${COPILOT_VERSION}\""),
        "the build does not fail when copilot --version disagrees with the pin"
    );
}

/// Without a newer version in the package cache, a run stays on the baked-in
/// release. The image sets the variable and each run adds the flag.
#[test]
fn the_image_disables_copilot_auto_update() {
    assert!(
        containerfile().contains("COPILOT_AUTO_UPDATE=false"),
        "the image does not disable Copilot auto-update"
    );
}

#[test]
fn the_image_creates_a_copilot_home_owned_by_the_worker() {
    let text = containerfile();
    assert!(
        text.contains("/copilot"),
        "the image has no /copilot directory"
    );
    let creates_owned = text
        .lines()
        .any(|line| line.contains("chown worker:worker") && line.contains("/copilot"));
    assert!(creates_owned, "/copilot is not owned by the worker user");
}

#[test]
fn the_image_still_installs_claude_and_codex() {
    let text = containerfile();
    assert!(
        text.contains("claude --version"),
        "the Copilot change dropped the Claude install"
    );
    assert!(
        text.contains("codex --version"),
        "the Copilot change dropped the Codex install"
    );
}

/// The one capturable run without a login: `copilot -p … --output-format json`
/// fails authentication and emits a single `result` with a non-zero `exitCode`
/// and no `assistant.message`. The reason goes to stderr, not the JSONL.
#[test]
fn the_unauthenticated_run_is_a_result_line_with_a_nonzero_exit_code() {
    let path = repo_root().join("crates/factory-runner/tests/fixtures/copilot-auth-failure.jsonl");
    let text = fs::read_to_string(path).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();

    assert_eq!(lines.len(), 1, "the failure emits one JSONL line");
    let result = &lines[0];
    assert_eq!(result["type"], "result");
    assert_ne!(
        result["exitCode"], 0,
        "an authentication failure exits non-zero"
    );
    assert!(
        result["usage"].is_object(),
        "the result carries a usage object even on an early failure"
    );
    let types: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();
    assert!(
        !types.contains(&"assistant.message"),
        "a failed run has no assistant message"
    );
}

#[test]
fn the_authenticated_run_carries_the_session_message_and_result() {
    let path = repo_root().join("crates/factory-runner/tests/fixtures/copilot-success.jsonl");
    let text = fs::read_to_string(path).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();

    let start = &lines[0];
    assert_eq!(start["type"], "session.start");
    assert!(
        start["data"]["sessionId"].as_str().is_some(),
        "session.start carries the session id"
    );

    let message = lines
        .iter()
        .rev()
        .find(|l| l["type"] == "assistant.message")
        .expect("the run has a final assistant message");
    assert_eq!(
        message["data"]["content"], "pong",
        "the final assistant message holds the reply under data.content"
    );

    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "session.usage_checkpoint"),
        "usage is reported in a session.usage_checkpoint"
    );

    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["exitCode"], 0, "a finished run exits zero");
}

fn pinned_copilot_version(text: &str) -> String {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("ARG COPILOT_VERSION="))
        .expect("the Containerfile pins a Copilot version")
        .trim()
        .to_owned()
}

/// Ignored by default: the build needs podman and the network it downloads
/// from, which the podman-free `just test` does not have, so it runs on demand
/// and on moon.
#[test]
#[ignore = "builds the worker image; needs podman and network"]
fn the_worker_image_builds_and_runs_copilot_as_the_worker() {
    let root = repo_root();
    let version = pinned_copilot_version(&containerfile());
    let tag = format!("factory-worker-copilot-test:{}", std::process::id());

    let build = Command::new("podman")
        .current_dir(&root)
        .args([
            "build",
            "-f",
            "containers/worker/Containerfile",
            "-t",
            tag.as_str(),
            ".",
        ])
        .status()
        .expect("podman is on PATH for the ignored image test");
    assert!(build.success(), "the worker image failed to build");

    let run = Command::new("podman")
        .args([
            "run",
            "--rm",
            tag.as_str(),
            "bash",
            "-c",
            r#"test "$(id -un)" = worker && copilot --version"#,
        ])
        .output()
        .expect("podman run");

    let _ = Command::new("podman")
        .args(["rmi", "-f", tag.as_str()])
        .status();

    assert!(
        run.status.success(),
        "copilot --version did not run as the worker: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains(version.as_str()),
        "the image runs copilot {version}, got: {stdout}"
    );
}
