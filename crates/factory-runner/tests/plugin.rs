use std::fs;
use std::path::{Path, PathBuf};

use factory_core::Task;
use factory_runner::{Expect, prompt};
use serde_json::{Value, json};

/// The repository root, two levels up from this crate's directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits under crates/<name>")
        .to_path_buf()
}

fn plugin_dir() -> PathBuf {
    repo_root().join("plugin")
}

#[test]
fn the_plugin_ships_the_review_skills_and_schema() {
    let plugin = plugin_dir();
    for file in [
        ".claude-plugin/plugin.json",
        "skills/review/SKILL.md",
        "skills/comment-review/SKILL.md",
        "schemas/review.json",
    ] {
        assert!(plugin.join(file).is_file(), "plugin is missing {file}");
    }
}

#[test]
fn the_manifest_names_the_factory_plugin() {
    let text = fs::read_to_string(plugin_dir().join(".claude-plugin/plugin.json")).unwrap();
    let manifest: Value = serde_json::from_str(&text).expect("plugin.json parses");
    assert_eq!(manifest["name"], "factory");
}

#[test]
fn each_skill_declares_its_name() {
    for name in ["review", "comment-review"] {
        let skill =
            fs::read_to_string(plugin_dir().join("skills").join(name).join("SKILL.md")).unwrap();
        assert!(
            skill.contains(&format!("name: {name}")),
            "the {name} skill does not name itself"
        );
    }
}

#[test]
fn the_review_schema_parses_and_offers_approve_or_changes() {
    let text = fs::read_to_string(plugin_dir().join("schemas/review.json")).unwrap();
    let schema: Value = serde_json::from_str(&text).expect("the review schema parses");
    let outcomes = schema["properties"]["outcome"]["enum"]
        .as_array()
        .expect("the outcome has an enum");
    assert!(outcomes.contains(&json!("approve")));
    assert!(outcomes.contains(&json!("changes")));
    assert!(
        schema["properties"]["feedback"].is_object(),
        "a changes verdict carries feedback"
    );
    assert_eq!(schema["required"], json!(["outcome"]));
}

#[test]
fn the_worker_image_bundles_the_plugin() {
    let containerfile =
        fs::read_to_string(repo_root().join("containers/worker/Containerfile")).unwrap();
    assert!(
        containerfile.contains("COPY") && containerfile.contains("plugin"),
        "the worker Containerfile does not copy the plugin into the image"
    );
    let install = fs::read_to_string(repo_root().join("deploy/install.sh")).unwrap();
    assert!(
        install.contains("containers/worker/Containerfile"),
        "install.sh must build the worker from the repository root so the COPY sees plugin/"
    );
}

#[test]
fn the_review_prompt_lets_the_agent_use_repository_skills() {
    let text = prompt(&Task::Review, Expect::Commits);
    assert!(text.contains("\"approve\""));
    assert!(text.contains("\"changes\""));
    assert!(
        text.to_lowercase().contains("skill"),
        "the review prompt does not mention the repository's skills"
    );
}
