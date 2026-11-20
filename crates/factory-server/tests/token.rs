//! Tests for the Claude token resolver: the one-time import, a later start
//! with a different env value, the env fallback when the store is unavailable,
//! and a usage refresh that picks up a newly saved token. No network and no
//! files: the store runs in memory.

use std::sync::Arc;

use factory_server::{
    ClaudeToken, LocalSecrets, MASTER_KEY_LEN, PlanUsageCache, SecretStore, Store,
    UnavailableSecrets, UsageSource, refresh_once,
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

/// A usage source that resolves its token through a [`ClaudeToken`] and returns
/// a body only when a token is set, standing in for the curl source so a test
/// can watch a refresh pick up a newly saved token with no network.
struct TokenSource {
    token: Arc<ClaudeToken>,
    body: String,
}

impl UsageSource for TokenSource {
    fn fetch(&self) -> impl std::future::Future<Output = Result<Option<String>, String>> {
        std::future::ready(Ok(self.token.resolve().map(|_| self.body.clone())))
    }
}

const SAMPLE: &str = r#"{
    "five_hour": { "utilization": 0.42, "resets_at": 1763660000 },
    "seven_day": { "utilization": 0.73, "resets_at": 1764200000 }
}"#;

#[tokio::test]
async fn a_refresh_picks_up_a_newly_saved_token() {
    let store = store();
    let secrets = local(&store);
    let token = Arc::new(ClaudeToken::new(Arc::clone(&secrets), None));
    let source = TokenSource {
        token: Arc::clone(&token),
        body: SAMPLE.into(),
    };
    let cache = PlanUsageCache::new();

    // With no token the fetch is skipped and the tile stays hidden.
    refresh_once(&source, &cache).await;
    assert!(cache.visible(now()).is_none());

    // Saving a token takes effect on the next refresh, with no restart.
    secrets.set_claude_token("sk-ant-oat01-saved").expect("set");
    refresh_once(&source, &cache).await;
    assert!(cache.visible(now()).is_some());
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}
