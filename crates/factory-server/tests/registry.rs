//! Tests for the agent registry: the Claude entry carries its credential,
//! worker image, config directory, settings page and stage-panel suggestions,
//! and the Codex entry is listed but reports that it cannot run.

use std::sync::Arc;

use factory_core::Agent;
use factory_server::{
    AgentRegistry, ClaudeToken, LocalSecrets, MASTER_KEY_LEN, SecretStore, Settings,
};

fn registry_with_token(token: Option<&str>) -> AgentRegistry {
    let store = Arc::new(factory_server::Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    if let Some(value) = token {
        secrets.set_claude_token(value).expect("store token");
    }
    AgentRegistry::new(Arc::new(ClaudeToken::new(secrets, None)))
}

#[test]
fn the_registry_lists_claude_and_codex_in_order() {
    let registry = registry_with_token(None);
    let ids: Vec<&str> = registry.entries().map(|entry| entry.id()).collect();
    assert_eq!(ids, ["claude", "codex"]);
}

#[test]
fn the_claude_entry_carries_its_image_and_suggestions() {
    let registry = registry_with_token(None);
    let claude = registry.entry(Agent::Claude);

    assert_eq!(claude.display_name(), "Claude");
    assert!(claude.available());
    assert_eq!(claude.settings_url(), Some("/settings/agents/claude"));
    assert_eq!(claude.host_config_dir(), Some("claude"));
    assert_eq!(
        claude.model_suggestions(),
        ["fable", "opus", "sonnet", "haiku"]
    );
    assert_eq!(claude.efforts(), ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(claude.default_effort(), "medium");

    let settings = Settings {
        worker_image: "localhost/factory-worker:latest".into(),
        ..Settings::default()
    };
    assert_eq!(
        claude.image(&settings),
        Some("localhost/factory-worker:latest".to_owned())
    );
}

#[test]
fn the_claude_entry_resolves_a_stored_token() {
    let with_token = registry_with_token(Some("sk-ant-oat01-stored"));
    assert_eq!(
        with_token.entry(Agent::Claude).credential(),
        Ok("sk-ant-oat01-stored".to_owned())
    );

    let without = registry_with_token(None);
    assert_eq!(
        without.entry(Agent::Claude).credential(),
        Err("no Claude token is set".to_owned())
    );
}

#[test]
fn the_codex_entry_is_listed_but_cannot_run() {
    let registry = registry_with_token(Some("sk-ant-oat01-stored"));
    let codex = registry.entry(Agent::Codex);

    assert_eq!(codex.display_name(), "Codex");
    assert!(!codex.available());
    assert_eq!(codex.settings_url(), None);
    assert_eq!(codex.host_config_dir(), None);
    assert_eq!(codex.model_suggestions(), [""; 0]);
    // Codex cannot run yet, but it carries its own effort levels so the stage
    // panel can offer them.
    assert_eq!(
        codex.efforts(),
        ["minimal", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(codex.default_effort(), "medium");
    assert_eq!(codex.image(&Settings::default()), None);
    // Codex cannot run even with a Claude token stored.
    assert!(codex.credential().is_err());
}
