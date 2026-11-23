//! Tests for the MCP server registry in the store. No network and no files:
//! the store runs in memory and the registry does only SQLite work.

use std::sync::Arc;

use factory_server::{
    LocalSecrets, MASTER_KEY_LEN, McpError, McpProjectSettings, McpServer, McpStages, McpTransport,
    SecretRef, SecretStore, Store,
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
        vec![McpServer::remote(
            "context7",
            "https://mcp.context7.com/mcp",
            headers(&["Authorization"]),
        )],
    );
    assert_eq!(
        store
            .mcp_server("context7")
            .expect("get")
            .map(|s| s.as_remote().unwrap().0.to_owned()),
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
    assert_eq!(server.as_remote().unwrap().1, Vec::<String>::new());
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
        store
            .mcp_server("context7")
            .expect("get")
            .map(|s| s.as_remote().unwrap().0.to_owned()),
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
    assert_eq!(
        server.as_remote().unwrap().1,
        headers(&["Authorization", "X-Api-Key"])
    );
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
    assert_eq!(
        server.as_remote().unwrap().0,
        "https://mcp.context7.com/mcp"
    );
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

fn stdio(command: &str, args: &[&str]) -> McpTransport {
    McpTransport::Stdio {
        command: command.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

#[test]
fn a_stdio_server_is_created_listed_and_read_back() {
    let store = store();
    store
        .create_mcp_server_with(
            "playwright",
            &stdio(" npx ", &["-y", "", "@playwright/mcp@0.0.83"]),
        )
        .expect("create stdio");
    let expected = McpServer::stdio(
        "playwright",
        "npx",
        vec!["-y".into(), "@playwright/mcp@0.0.83".into()],
    );
    assert_eq!(
        store.mcp_server("playwright").unwrap(),
        Some(expected.clone())
    );
    assert_eq!(store.mcp_servers().unwrap(), vec![expected]);
}

#[test]
fn an_invalid_stdio_server_writes_no_row() {
    let store = store();
    let too_many: Vec<&str> = vec!["a"; 65];
    let long = "x".repeat(4097);
    let bad: Vec<McpTransport> = vec![
        stdio("", &[]),
        stdio("  ", &[]),
        stdio("np\nx", &[]),
        stdio("np\rx", &[]),
        stdio("np\0x", &[]),
        stdio(&"c".repeat(257), &[]),
        stdio("${HOME}/bin", &[]),
        stdio("npx", &too_many),
        stdio("npx", &[&long]),
        stdio("npx", &["a\0b"]),
        stdio("npx", &["${HOME}"]),
    ];
    for transport in &bad {
        let result = store.create_mcp_server_with("playwright", transport);
        assert!(
            matches!(result, Err(McpError::BadCommand(_) | McpError::BadArgs(_))),
            "{transport:?} should be rejected, got {result:?}",
        );
    }
    assert_eq!(store.mcp_servers().unwrap(), Vec::new());
    // 64 arguments and a lone `$` are fine.
    let sixty_four: Vec<&str> = vec!["a"; 64];
    store
        .create_mcp_server_with("playwright", &stdio("npx", &sixty_four))
        .expect("64 arguments");
}

#[test]
fn a_stdio_name_clashes_with_a_remote_one() {
    let store = store();
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    let again = store.create_mcp_server_with("context7", &stdio("npx", &[]));
    assert!(matches!(again, Err(McpError::DuplicateName(name)) if name == "context7"));
    assert!(
        store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .is_some()
    );
}

#[test]
fn stdio_arguments_keep_spaces_quotes_and_a_lone_dollar() {
    let store = store();
    let args = [
        "--title=a \"b\" 'c'",
        " lead and trail ",
        "$",
        "cost $5",
        "back\\slash",
    ];
    store
        .create_mcp_server_with("playwright", &stdio("npx", &args))
        .unwrap();
    let server = store.mcp_server("playwright").unwrap().unwrap();
    assert_eq!(server.as_stdio().unwrap().1, args);
}

#[test]
fn editing_a_stdio_server_keeps_project_settings_and_cannot_change_its_kind() {
    let store = store();
    store
        .create_mcp_server_with("playwright", &stdio("npx", &["-y", "a"]))
        .unwrap();
    let on = McpProjectSettings {
        enabled: true,
        stages: McpStages {
            implement: true,
            ..McpStages::default()
        },
    };
    store
        .save_project_mcp("maestro", "playwright", &on)
        .unwrap();

    assert_eq!(
        store
            .mcp_edit_deletes("playwright", "https://x.test/mcp", &[])
            .unwrap(),
        Vec::<(String, String)>::new()
    );
    store
        .edit_mcp_server_with("playwright", &stdio("uvx", &["b"]))
        .unwrap();
    let server = store.mcp_server("playwright").unwrap().unwrap();
    assert_eq!(server.as_stdio().unwrap(), ("uvx", &["b".to_owned()][..]));
    assert_eq!(store.project_mcp("maestro", "playwright").unwrap(), on);

    let remote = store.edit_mcp_server("playwright", "https://x.test/mcp", &[]);
    assert!(matches!(remote, Err(McpError::KindChange(_))));
    let bad = store.edit_mcp_server_with("playwright", &stdio("", &[]));
    assert!(matches!(bad, Err(McpError::BadCommand(_))));
    assert_eq!(store.mcp_server("playwright").unwrap(), Some(server));

    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    let to_stdio = store.edit_mcp_server_with("context7", &stdio("npx", &[]));
    assert!(matches!(to_stdio, Err(McpError::KindChange(_))));
}

#[test]
fn deleting_a_stdio_server_removes_its_project_rows() {
    let store = store();
    store
        .create_mcp_server_with("playwright", &stdio("npx", &[]))
        .unwrap();
    let on = McpProjectSettings {
        enabled: true,
        stages: McpStages {
            spec: true,
            ..McpStages::default()
        },
    };
    store
        .save_project_mcp("maestro", "playwright", &on)
        .unwrap();
    store.delete_mcp_server("playwright").unwrap();
    assert!(store.mcp_server("playwright").unwrap().is_none());
    assert_eq!(
        store.project_mcp("maestro", "playwright").unwrap(),
        McpProjectSettings::default()
    );
}

#[test]
fn a_database_from_before_stdio_loads_every_server_as_remote() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE mcp_servers (
                 name TEXT PRIMARY KEY,
                 url TEXT NOT NULL,
                 headers TEXT NOT NULL
             );
             INSERT INTO mcp_servers (name, url, headers)
             VALUES ('context7', 'https://mcp.context7.com/mcp', '[\"Authorization\"]');",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.mcp_server("context7").unwrap(),
        Some(McpServer::remote(
            "context7",
            "https://mcp.context7.com/mcp",
            vec!["Authorization".into()]
        )),
    );
    store
        .create_mcp_server_with("playwright", &stdio("npx", &[]))
        .unwrap();
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn stdio_store() -> Arc<Store> {
    let store = store();
    store
        .create_mcp_server_with(
            "playwright",
            &McpTransport::Stdio {
                command: "npx".to_owned(),
                args: strings(&["-y", "pkg"]),
            },
        )
        .expect("create");
    store
}

#[test]
fn project_mcp_args_keep_order_and_default_to_empty() {
    let store = stdio_store();
    assert_eq!(
        store.project_mcp_args("maestro", "playwright").unwrap(),
        Vec::<String>::new()
    );
    let args = strings(&["--headless", "a b", "--x=1"]);
    store
        .save_project_mcp_args("maestro", "playwright", &args)
        .unwrap();
    assert_eq!(
        store.project_mcp_args("maestro", "playwright").unwrap(),
        args
    );
}

#[test]
fn saving_one_projects_args_leaves_other_projects_alone() {
    let store = stdio_store();
    store
        .save_project_mcp_args("maestro", "playwright", &strings(&["--headless"]))
        .unwrap();
    store
        .save_project_mcp_args("tool", "playwright", &strings(&["--caps=vision"]))
        .unwrap();
    store
        .save_project_mcp_args("maestro", "playwright", &strings(&["--x=1"]))
        .unwrap();
    assert_eq!(
        store.project_mcp_args("tool", "playwright").unwrap(),
        ["--caps=vision"]
    );
}

#[test]
fn turning_a_server_off_keeps_the_args() {
    let store = stdio_store();
    let on = McpProjectSettings {
        enabled: true,
        stages: only("implement"),
    };
    store
        .save_project_mcp("maestro", "playwright", &on)
        .unwrap();
    store
        .save_project_mcp_args("maestro", "playwright", &strings(&["--headless"]))
        .unwrap();
    store
        .save_project_mcp("maestro", "playwright", &McpProjectSettings::default())
        .unwrap();
    store
        .save_project_mcp("maestro", "playwright", &on)
        .unwrap();
    assert_eq!(
        store.project_mcp_args("maestro", "playwright").unwrap(),
        ["--headless"]
    );
}

#[test]
fn deleting_a_server_or_removing_a_project_deletes_its_args() {
    let store = stdio_store();
    store
        .create_project(
            "maestro",
            factory_runner::ForgeKind::GitHub,
            "o/m",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    store
        .save_project_mcp_args("maestro", "playwright", &strings(&["--a"]))
        .unwrap();
    store
        .save_project_mcp_args("tool", "playwright", &strings(&["--b"]))
        .unwrap();
    store.remove_project("maestro").unwrap();
    assert_eq!(
        store.project_mcp_args("maestro", "playwright").unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        store.project_mcp_args("tool", "playwright").unwrap(),
        ["--b"]
    );
    store.delete_mcp_server("playwright").unwrap();
    assert_eq!(
        store.project_mcp_args("tool", "playwright").unwrap(),
        Vec::<String>::new()
    );
}
