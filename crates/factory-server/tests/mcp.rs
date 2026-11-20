//! Tests for the MCP server registry in the store. No network and no files:
//! the store runs in memory and the registry does only SQLite work.

use std::sync::Arc;

use factory_server::{
    LocalSecrets, MASTER_KEY_LEN, McpError, McpServer, SecretRef, SecretStore, Store,
};

fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().expect("open in-memory store"))
}

fn headers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn key() -> [u8; MASTER_KEY_LEN] {
    [7; MASTER_KEY_LEN]
}

fn secret_at<'a>(project: &'a str, server: &'a str, header: &'a str) -> SecretRef<'a> {
    SecretRef {
        project,
        server,
        header,
    }
}

#[test]
fn create_lists_and_gets_a_server() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create");

    assert_eq!(
        store.mcp_servers().expect("list"),
        vec![McpServer {
            name: "context7".to_owned(),
            url: "https://mcp.context7.com/mcp".to_owned(),
            headers: headers(&["Authorization"]),
        }],
    );
    assert_eq!(
        store.mcp_server("context7").expect("get").map(|s| s.url),
        Some("https://mcp.context7.com/mcp".to_owned()),
    );
    assert!(store.mcp_server("absent").expect("get absent").is_none());
}

#[test]
fn a_keyless_server_stores_no_headers() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .expect("create keyless");
    let server = store.mcp_server("context7").expect("get").expect("present");
    assert_eq!(server.headers, Vec::<String>::new());
}

#[test]
fn duplicate_names_are_rejected() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .expect("first create");
    let again = store.create_mcp_server("context7", "https://other.example.com/mcp", &[]);
    assert!(matches!(again, Err(McpError::DuplicateName(name)) if name == "context7"));
    // The first server is untouched.
    assert_eq!(
        store.mcp_server("context7").expect("get").map(|s| s.url),
        Some("https://mcp.context7.com/mcp".to_owned()),
    );
}

#[test]
fn a_bad_name_is_rejected() {
    let store = store();
    for name in ["", "Context7", "has space", "under_score", &"x".repeat(33)] {
        assert!(
            matches!(
                store.create_mcp_server(name, "https://mcp.context7.com/mcp", &[]),
                Err(McpError::BadName(_)),
            ),
            "name {name:?} should be rejected",
        );
    }
    // A 32-character name of the allowed set is accepted.
    store
        .create_mcp_server(&"a".repeat(32), "https://mcp.context7.com/mcp", &[])
        .expect("32-character name");
}

#[test]
fn a_non_https_url_is_rejected() {
    let store = store();
    for url in [
        "http://mcp.context7.com/mcp",
        "ftp://mcp.context7.com/mcp",
        "not a url",
        "https:///mcp",
    ] {
        assert!(
            matches!(
                store.create_mcp_server("context7", url, &[]),
                Err(McpError::BadUrl(_)),
            ),
            "url {url:?} should be rejected",
        );
    }
}

#[test]
fn a_reserved_or_malformed_header_is_rejected() {
    let store = store();
    // Reserved, case-insensitively.
    for header in [
        "Host",
        "content-length",
        "Mcp-Session-Id",
        "MCP-Protocol-Version",
    ] {
        assert!(
            matches!(
                store.create_mcp_server(
                    "context7",
                    "https://mcp.context7.com/mcp",
                    &headers(&[header])
                ),
                Err(McpError::ReservedHeader(_)),
            ),
            "header {header:?} should be reserved",
        );
    }
    // Not an HTTP token.
    for header in ["has space", "bad:colon", "nul\0byte"] {
        assert!(
            matches!(
                store.create_mcp_server(
                    "context7",
                    "https://mcp.context7.com/mcp",
                    &headers(&[header])
                ),
                Err(McpError::BadHeader(_)),
            ),
            "header {header:?} should be malformed",
        );
    }
}

#[test]
fn duplicate_header_names_are_deduplicated() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization", "authorization", "X-Api-Key"]),
        )
        .expect("create");
    let server = store.mcp_server("context7").expect("get").expect("present");
    assert_eq!(server.headers, headers(&["Authorization", "X-Api-Key"]));
}

#[test]
fn editing_a_missing_server_is_not_found() {
    let store = store();
    let result = store.edit_mcp_server("absent", "https://mcp.context7.com/mcp", &[]);
    assert!(matches!(result, Err(McpError::NotFound(name)) if name == "absent"));
}

#[test]
fn an_edit_validates_the_new_fields() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .expect("create");
    assert!(matches!(
        store.edit_mcp_server("context7", "http://mcp.context7.com/mcp", &[]),
        Err(McpError::BadUrl(_)),
    ));
    assert!(matches!(
        store.edit_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Host"])
        ),
        Err(McpError::ReservedHeader(_)),
    ));
    // The rejected edits left the server as it was.
    let server = store.mcp_server("context7").expect("get").expect("present");
    assert_eq!(server.url, "https://mcp.context7.com/mcp");
}

#[test]
fn changing_only_the_path_keeps_stored_values() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create");
    let secrets = LocalSecrets::new(&key(), Arc::clone(&store));
    secrets
        .put(
            secret_at("maestro", "context7", "Authorization"),
            "Bearer sk-x",
        )
        .expect("store a value");

    store
        .edit_mcp_server(
            "context7",
            "https://mcp.context7.com/v2/mcp",
            &headers(&["Authorization"]),
        )
        .expect("edit path only");

    assert_eq!(
        secrets
            .get(secret_at("maestro", "context7", "Authorization"))
            .expect("get")
            .as_deref(),
        Some("Bearer sk-x"),
        "a path-only change keeps the stored value",
    );
}

#[test]
fn changing_the_origin_deletes_stored_values() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create");
    let secrets = LocalSecrets::new(&key(), Arc::clone(&store));
    secrets
        .put(
            secret_at("maestro", "context7", "Authorization"),
            "Bearer sk-x",
        )
        .expect("store a value");

    store
        .edit_mcp_server(
            "context7",
            "https://evil.example.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("edit origin");

    assert!(
        secrets
            .get(secret_at("maestro", "context7", "Authorization"))
            .expect("get")
            .is_none(),
        "an origin change deletes every stored value",
    );
}

#[test]
fn removing_a_header_deletes_only_that_headers_values() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization", "X-Api-Key"]),
        )
        .expect("create");
    let secrets = LocalSecrets::new(&key(), Arc::clone(&store));
    secrets
        .put(
            secret_at("maestro", "context7", "Authorization"),
            "Bearer sk-x",
        )
        .expect("store auth");
    secrets
        .put(secret_at("maestro", "context7", "X-Api-Key"), "key-value")
        .expect("store api key");

    store
        .edit_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("remove X-Api-Key");

    assert_eq!(
        secrets
            .get(secret_at("maestro", "context7", "Authorization"))
            .expect("get auth")
            .as_deref(),
        Some("Bearer sk-x"),
        "the kept header's value stays",
    );
    assert!(
        secrets
            .get(secret_at("maestro", "context7", "X-Api-Key"))
            .expect("get api key")
            .is_none(),
        "the removed header's value is gone",
    );
}

#[test]
fn deleting_a_server_removes_it_and_its_secrets() {
    let store = store();
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create");
    let secrets = LocalSecrets::new(&key(), Arc::clone(&store));
    secrets
        .put(
            secret_at("maestro", "context7", "Authorization"),
            "Bearer sk-x",
        )
        .expect("store a value");

    store.delete_mcp_server("context7").expect("delete");

    assert!(store.mcp_server("context7").expect("get").is_none());
    assert!(
        secrets
            .get(secret_at("maestro", "context7", "Authorization"))
            .expect("get")
            .is_none(),
        "deleting a server deletes its stored values",
    );
    // Deleting a server that is not there is fine.
    store.delete_mcp_server("context7").expect("delete again");
}
