//! Tests for the encrypted secret store. No network and no files: the store
//! runs in memory and the cipher is deterministic given its key.

use std::sync::Arc;

use factory_server::{
    LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN, SecretError, SecretRef, SecretStore, Store,
    UnavailableSecrets,
};

fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().expect("open in-memory store"))
}

fn key(seed: u8) -> [u8; MASTER_KEY_LEN] {
    [seed; MASTER_KEY_LEN]
}

fn at<'a>(project: &'a str, server: &'a str, header: &'a str) -> SecretRef<'a> {
    SecretRef {
        project,
        server,
        header,
    }
}

#[test]
fn put_get_delete_round_trips() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let here = at("maestro", "context7", "Authorization");

    assert!(secrets.get(here).expect("get empty").is_none());

    secrets.put(here, "Bearer sk-secret-value").expect("put");
    assert_eq!(
        secrets.get(here).expect("get stored").as_deref(),
        Some("Bearer sk-secret-value"),
    );

    // Putting again replaces the value in place.
    secrets.put(here, "Bearer sk-second").expect("replace");
    assert_eq!(
        secrets.get(here).expect("get replaced").as_deref(),
        Some("Bearer sk-second"),
    );

    secrets.delete(here).expect("delete");
    assert!(secrets.get(here).expect("get deleted").is_none());
    // Deleting a value that is not there is fine.
    secrets.delete(here).expect("delete again");
}

#[test]
fn stored_row_holds_no_plaintext() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let here = at("maestro", "context7", "CONTEXT7_API_KEY");
    let value = "ctx7-plaintext-credential";

    secrets.put(here, value).expect("put");

    let (backend, stored) = store
        .get_secret(here)
        .expect("raw row")
        .expect("row exists");
    assert_eq!(backend, LOCAL_BACKEND);
    assert!(
        !contains(&stored, value.as_bytes()),
        "the plaintext must not appear in the stored bytes",
    );
}

#[test]
fn a_different_key_fails_to_decrypt() {
    let store = store();
    let here = at("maestro", "context7", "Authorization");

    LocalSecrets::new(&key(1), Arc::clone(&store))
        .put(here, "Bearer sk-secret")
        .expect("put with the first key");

    let other = LocalSecrets::new(&key(2), Arc::clone(&store));
    assert!(matches!(other.get(here), Err(SecretError::Decrypt)));
}

#[test]
fn a_ciphertext_moved_to_another_row_fails() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let source = at("maestro", "context7", "Authorization");
    secrets.put(source, "Bearer sk-secret").expect("put");

    let (backend, stored) = store
        .get_secret(source)
        .expect("raw row")
        .expect("row exists");

    // The same ciphertext under a different project, server or header must not
    // decrypt, because each is bound into the associated data.
    for moved in [
        at("other-project", "context7", "Authorization"),
        at("maestro", "other-server", "Authorization"),
        at("maestro", "context7", "Other-Header"),
    ] {
        store
            .put_secret(moved, &backend, &stored)
            .expect("plant the moved ciphertext");
        assert!(
            matches!(secrets.get(moved), Err(SecretError::Decrypt)),
            "a ciphertext from {source:?} must not decrypt at {moved:?}",
        );
    }

    // The original row still decrypts.
    assert_eq!(
        secrets.get(source).expect("get source").as_deref(),
        Some("Bearer sk-secret"),
    );
}

fn headers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn claude_token_round_trips() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));

    assert!(secrets.claude_token().expect("read empty").is_none());

    secrets.set_claude_token("sk-ant-oat01-first").expect("set");
    assert_eq!(
        secrets.claude_token().expect("read stored").as_deref(),
        Some("sk-ant-oat01-first"),
    );

    // Setting again replaces the token in place.
    secrets
        .set_claude_token("sk-ant-oat01-second")
        .expect("replace");
    assert_eq!(
        secrets.claude_token().expect("read replaced").as_deref(),
        Some("sk-ant-oat01-second"),
    );

    secrets.clear_claude_token().expect("clear");
    assert!(secrets.claude_token().expect("read cleared").is_none());
    // Clearing a token that is not there is fine.
    secrets.clear_claude_token().expect("clear again");
}

#[test]
fn claude_token_row_holds_no_plaintext() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let value = "sk-ant-oat01-plaintext";

    secrets.set_claude_token(value).expect("set");

    let (backend, stored) = store
        .get_secret(at("", "", "claude-token"))
        .expect("raw row")
        .expect("row exists");
    assert_eq!(backend, LOCAL_BACKEND);
    assert!(
        !contains(&stored, value.as_bytes()),
        "the plaintext must not appear in the stored bytes",
    );
}

#[test]
fn deleting_an_mcp_server_named_claude_keeps_the_token() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    store
        .create_mcp_server(
            "claude",
            "https://mcp.example.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create server");
    secrets
        .put(
            at("maestro", "claude", "Authorization"),
            "Bearer header-value",
        )
        .expect("store the server header");
    secrets
        .set_claude_token("sk-ant-oat01-token")
        .expect("set token");

    store.delete_mcp_server("claude").expect("delete server");

    assert_eq!(
        secrets.claude_token().expect("read token").as_deref(),
        Some("sk-ant-oat01-token"),
        "deleting an MCP server named claude must leave the token",
    );
    assert!(
        secrets
            .get(at("maestro", "claude", "Authorization"))
            .expect("read header")
            .is_none(),
        "the server's own header value is gone with the server",
    );
}

#[test]
fn renaming_an_mcp_server_named_claude_keeps_the_token() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    store
        .create_mcp_server(
            "claude",
            "https://mcp.example.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create server");
    secrets
        .put(
            at("maestro", "claude", "Authorization"),
            "Bearer header-value",
        )
        .expect("store the server header");
    secrets
        .set_claude_token("sk-ant-oat01-token")
        .expect("set token");

    // An origin change deletes every stored value for the server, the broadest
    // secret delete an MCP edit runs.
    store
        .edit_mcp_server(
            "claude",
            "https://other.example.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("edit origin");

    assert_eq!(
        secrets.claude_token().expect("read token").as_deref(),
        Some("sk-ant-oat01-token"),
        "an MCP server edit must leave the token",
    );
}

#[test]
fn a_ciphertext_swapped_with_an_mcp_header_fails() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let header = at("maestro", "context7", "Authorization");
    secrets
        .put(header, "Bearer sk-header")
        .expect("store header");
    secrets
        .set_claude_token("sk-ant-oat01-token")
        .expect("set token");

    let token_ref = at("", "", "claude-token");
    let (header_backend, header_bytes) = store
        .get_secret(header)
        .expect("raw header row")
        .expect("header row exists");
    let (token_backend, token_bytes) = store
        .get_secret(token_ref)
        .expect("raw token row")
        .expect("token row exists");

    // The header ciphertext planted in the token row does not decrypt as the
    // token, and the token ciphertext planted in the header row does not
    // decrypt as the header, because each binds different associated data.
    store
        .put_secret(token_ref, &header_backend, &header_bytes)
        .expect("plant header bytes in the token row");
    assert!(matches!(secrets.claude_token(), Err(SecretError::Decrypt)));

    store
        .put_secret(header, &token_backend, &token_bytes)
        .expect("plant token bytes in the header row");
    assert!(matches!(secrets.get(header), Err(SecretError::Decrypt)));
}

#[test]
fn an_unavailable_store_reports_the_token_unavailable() {
    let store = store();
    let secrets = UnavailableSecrets::new(Arc::clone(&store));

    assert!(matches!(
        secrets.claude_token(),
        Err(SecretError::Unavailable)
    ));
    assert!(matches!(
        secrets.set_claude_token("sk-ant-oat01-token"),
        Err(SecretError::Unavailable)
    ));
    // Clearing still works, so a token can be removed after the key is lost.
    secrets.clear_claude_token().expect("clear");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
