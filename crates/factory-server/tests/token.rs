//! Tests for the Claude token resolver: the one-time import, a later start
//! with a different env value, and the env fallback when the store is
//! unavailable. No network and no files: the store runs in memory.

use std::sync::Arc;

use factory_server::{
    ClaudeToken, LocalSecrets, MASTER_KEY_LEN, SecretStore, Store, UnavailableSecrets,
};

fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().expect("open in-memory store"))
}

fn key(seed: u8) -> [u8; MASTER_KEY_LEN] {
    [seed; MASTER_KEY_LEN]
}

fn local(store: &Arc<Store>) -> Arc<dyn SecretStore> {
    Arc::new(LocalSecrets::new(&key(1), Arc::clone(store)))
}

#[test]
fn import_stores_the_env_token_once() {
    let store = store();
    let secrets = local(&store);
    assert!(secrets.claude_token().expect("read empty").is_none());

    let first = ClaudeToken::new(Arc::clone(&secrets), Some("sk-ant-oat01-env".into()));
    first.import_env_token();

    assert_eq!(
        secrets.claude_token().expect("read imported").as_deref(),
        Some("sk-ant-oat01-env"),
    );
    assert_eq!(first.resolve().as_deref(), Some("sk-ant-oat01-env"));
}

#[test]
fn a_later_start_with_a_different_env_keeps_the_stored_token() {
    let store = store();
    let secrets = local(&store);

    ClaudeToken::new(Arc::clone(&secrets), Some("sk-ant-oat01-first".into())).import_env_token();

    // A second start, with the token already stored, must ignore the env var
    // and leave the stored token alone.
    let second = ClaudeToken::new(Arc::clone(&secrets), Some("sk-ant-oat01-second".into()));
    second.import_env_token();

    assert_eq!(
        secrets.claude_token().expect("read stored").as_deref(),
        Some("sk-ant-oat01-first"),
    );
    assert_eq!(second.resolve().as_deref(), Some("sk-ant-oat01-first"));
}

#[test]
fn an_empty_store_and_no_env_resolves_to_no_token() {
    let store = store();
    let secrets = local(&store);

    let token = ClaudeToken::new(secrets, None);
    token.import_env_token();

    assert!(token.resolve().is_none());
}

#[test]
fn an_unavailable_store_falls_back_to_the_env_token() {
    let store = store();
    let secrets: Arc<dyn SecretStore> = Arc::new(UnavailableSecrets::new(store));

    let token = ClaudeToken::new(Arc::clone(&secrets), Some("sk-ant-oat01-fallback".into()));
    // The import is a no-op while the store is unavailable, so nothing is
    // written, but the fallback resolves to the env token.
    token.import_env_token();

    assert_eq!(token.resolve().as_deref(), Some("sk-ant-oat01-fallback"));
}

#[test]
fn an_unavailable_store_with_no_env_resolves_to_no_token() {
    let store = store();
    let secrets: Arc<dyn SecretStore> = Arc::new(UnavailableSecrets::new(store));

    let token = ClaudeToken::new(secrets, None);

    assert!(token.resolve().is_none());
}
