//! Tests for the MCP server registry in the store. No network and no files:
//! the store runs in memory and the registry does only SQLite work.

use std::sync::Arc;

use factory_server::{
    LocalSecrets, MASTER_KEY_LEN, McpError, McpProjectSettings, McpServer, McpStages, SecretRef,
    SecretStore, Store,
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

fn only(stage: &str) -> McpStages {
    McpStages {
        clarify: stage == "clarify",
        spec: stage == "spec",
        plan: stage == "plan",
        implement: stage == "implement",
        review: stage == "review",
    }
}

#[test]
fn a_project_with_no_row_reads_a_server_off() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .expect("create");
    // A newly added server is off for a project with no stored row, with no
    // stage picked.
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        McpProjectSettings::default(),
    );
    // A server that was never added reads the same default.
    assert_eq!(
        store.project_mcp("maestro", "absent").expect("read"),
        McpProjectSettings::default(),
    );
}

#[test]
fn enabling_a_server_stores_its_stages() {
    let store = store();
    let settings = McpProjectSettings {
        enabled: true,
        stages: only("clarify"),
    };
    store
        .save_project_mcp("maestro", "context7", &settings)
        .expect("save");
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        settings,
    );
}

#[test]
fn enabling_a_server_with_no_stage_is_rejected_and_not_stored() {
    let store = store();
    let bad = McpProjectSettings {
        enabled: true,
        stages: McpStages::default(),
    };
    let err = store
        .save_project_mcp("maestro", "context7", &bad)
        .expect_err("no stage");
    assert!(
        matches!(&err, McpError::NoStage(name) if name == "context7"),
        "{err:?}"
    );
    // Nothing was written, so the project still reads the default.
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        McpProjectSettings::default(),
    );
}

#[test]
fn a_disabled_server_may_keep_no_stage() {
    let store = store();
    // Off with no stage is the resting state and must save, so turning a server
    // off does not trip the no-stage rule.
    store
        .save_project_mcp("maestro", "context7", &McpProjectSettings::default())
        .expect("save off");
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        McpProjectSettings::default(),
    );
}

#[test]
fn saving_one_projects_mcp_leaves_other_projects_alone() {
    let store = store();
    let maestro = McpProjectSettings {
        enabled: true,
        stages: only("review"),
    };
    let tool = McpProjectSettings {
        enabled: true,
        stages: only("plan"),
    };
    store
        .save_project_mcp("maestro", "context7", &maestro)
        .expect("save maestro");
    store
        .save_project_mcp("tool", "context7", &tool)
        .expect("save tool");
    // Rewriting maestro does not touch tool.
    store
        .save_project_mcp("maestro", "context7", &maestro)
        .expect("save maestro again");
    assert_eq!(store.project_mcp("tool", "context7").expect("read"), tool);
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        maestro,
    );
}

#[test]
fn deleting_a_server_removes_its_project_settings() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .expect("create");
    store
        .save_project_mcp(
            "maestro",
            "context7",
            &McpProjectSettings {
                enabled: true,
                stages: only("clarify"),
            },
        )
        .expect("enable");

    store.delete_mcp_server("context7").expect("delete");

    // The project's row for the server is gone, so it reads the default again.
    assert_eq!(
        store.project_mcp("maestro", "context7").expect("read"),
        McpProjectSettings::default(),
    );
}

#[test]
fn has_secret_reports_a_stored_value_without_the_value() {
    let store = store();
    let at = secret_at("maestro", "context7", "Authorization");
    assert!(!store.has_secret(at).expect("absent"));
    let secrets = LocalSecrets::new(&key(), Arc::clone(&store));
    secrets.put(at, "Bearer sk-x").expect("store");
    assert!(store.has_secret(at).expect("present"));
    secrets.delete(at).expect("clear");
    assert!(!store.has_secret(at).expect("cleared"));
}
