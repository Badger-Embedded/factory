//! Tests for the encrypted secret store. No network and no files: the store
//! runs in memory and the cipher is deterministic given its key.

use std::sync::Arc;

use factory_runner::{ForgeAccount, ForgeKind};

use factory_server::{
    CodexAuthError, CopilotTokenError, ForgeTokenOwner, LOCAL_BACKEND, LocalSecrets,
    MASTER_KEY_LEN, SecretError, SecretRef, SecretStore, Store, UnavailableSecrets,
    validate_codex_auth, validate_copilot_token,
};

fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().expect("open in-memory store"))
}

fn account() -> ForgeAccount {
    ForgeAccount {
        login: "bot".into(),
        id: 5,
    }
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

/// A minimal subscription `auth.json`, the shape a `ChatGPT` Plus or Pro `codex
/// login` writes: an object with string tokens and a `last_refresh`.
fn auth_json(access: &str, refresh: &str) -> String {
    format!(
        r#"{{"tokens":{{"access_token":"{access}","refresh_token":"{refresh}","account_id":"acc-1"}},"last_refresh":"2026-11-21T00:00:00Z"}}"#
    )
}

#[test]
fn codex_auth_round_trips() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));

    assert!(secrets.codex_auth().expect("read empty").is_none());

    let first = auth_json("access-first", "refresh-first");
    secrets.set_codex_auth(&first).expect("set");
    assert_eq!(
        secrets.codex_auth().expect("read stored").as_deref(),
        Some(first.as_str()),
    );

    // Setting again replaces the login in place.
    let second = auth_json("access-second", "refresh-second");
    secrets.set_codex_auth(&second).expect("replace");
    assert_eq!(
        secrets.codex_auth().expect("read replaced").as_deref(),
        Some(second.as_str()),
    );

    secrets.clear_codex_auth().expect("clear");
    assert!(secrets.codex_auth().expect("read cleared").is_none());
    // Clearing a login that is not there is fine.
    secrets.clear_codex_auth().expect("clear again");
}

#[test]
fn codex_auth_row_holds_no_plaintext() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let value = auth_json("access-plaintext", "refresh-plaintext");

    secrets.set_codex_auth(&value).expect("set");

    let (backend, stored) = store
        .get_secret(at("", "", "codex-auth"))
        .expect("raw row")
        .expect("row exists");
    assert_eq!(backend, LOCAL_BACKEND);
    assert!(
        !contains(&stored, b"access-plaintext") && !contains(&stored, b"refresh-plaintext"),
        "no token from the login may appear in the stored bytes",
    );
}

#[test]
fn deleting_an_mcp_server_named_codex_keeps_the_login() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    store
        .create_mcp_server(
            "codex",
            "https://mcp.example.com/mcp",
            &headers(&["Authorization"]),
        )
        .expect("create server");
    secrets
        .put(
            at("maestro", "codex", "Authorization"),
            "Bearer header-value",
        )
        .expect("store the server header");
    let login = auth_json("access-token", "refresh-token");
    secrets.set_codex_auth(&login).expect("set login");

    store.delete_mcp_server("codex").expect("delete server");

    assert_eq!(
        secrets.codex_auth().expect("read login").as_deref(),
        Some(login.as_str()),
        "deleting an MCP server named codex must leave the login",
    );
    assert!(
        secrets
            .get(at("maestro", "codex", "Authorization"))
            .expect("read header")
            .is_none(),
        "the server's own header value is gone with the server",
    );
}

#[test]
fn a_ciphertext_moved_between_the_reserved_and_header_rows_fails() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let header = at("maestro", "context7", "Authorization");
    let claude = at("", "", "claude-token");
    let codex = at("", "", "codex-auth");
    let copilot = at("", "", "copilot-token");
    let local = at("", "", "local-key");

    secrets
        .put(header, "Bearer sk-header")
        .expect("store header");
    secrets
        .set_claude_token("sk-ant-oat01-token")
        .expect("set token");
    secrets
        .set_codex_auth(&auth_json("access-token", "refresh-token"))
        .expect("set login");
    secrets
        .set_copilot_token("gho_copilottoken")
        .expect("set copilot token");
    secrets.set_local_key("sk-local").expect("set local key");

    let raw = |reference| {
        store
            .get_secret(reference)
            .expect("raw row")
            .expect("row exists")
    };
    let rows = [
        ("header", header),
        ("claude", claude),
        ("codex", codex),
        ("copilot", copilot),
        ("local", local),
    ];
    for (source_name, source) in rows {
        let (backend, bytes) = raw(source);
        for (dest_name, dest) in rows {
            if dest_name == source_name {
                continue;
            }
            store
                .put_secret(dest, &backend, &bytes)
                .expect("plant the moved ciphertext");
            let moved = match dest_name {
                "header" => secrets.get(dest),
                "claude" => secrets.claude_token(),
                "codex" => secrets.codex_auth(),
                "copilot" => secrets.copilot_token(),
                "local" => secrets.local_key(),
                _ => unreachable!(),
            };
            assert!(
                matches!(moved, Err(SecretError::Decrypt)),
                "the {source_name} ciphertext must not decrypt in the {dest_name} row",
            );
            // Put the destination's own value back so the next source reads the
            // real ciphertext rather than the planted one.
            let restore = match dest_name {
                "header" => secrets.put(dest, "Bearer sk-header"),
                "claude" => secrets.set_claude_token("sk-ant-oat01-token"),
                "codex" => secrets.set_codex_auth(&auth_json("access-token", "refresh-token")),
                "copilot" => secrets.set_copilot_token("gho_copilottoken"),
                "local" => secrets.set_local_key("sk-local"),
                _ => unreachable!(),
            };
            restore.expect("restore the destination row");
        }
    }
}

#[test]
fn an_unavailable_store_reports_the_codex_login_unavailable() {
    let store = store();
    let secrets = UnavailableSecrets::new(Arc::clone(&store));

    assert!(matches!(
        secrets.codex_auth(),
        Err(SecretError::Unavailable)
    ));
    assert!(matches!(
        secrets.set_codex_auth(&auth_json("access-token", "refresh-token")),
        Err(SecretError::Unavailable)
    ));
    // Clearing still works, so a login can be removed after the key is lost.
    secrets.clear_codex_auth().expect("clear");
}

#[test]
fn validate_codex_auth_accepts_a_subscription_login() {
    validate_codex_auth(&auth_json("access-token", "refresh-token")).expect("valid login");
}

#[test]
fn validate_codex_auth_refuses_non_subscription_files() {
    // Not JSON, and a JSON value that is not an object, both land on NotJsonObject.
    assert_eq!(
        validate_codex_auth("not json at all"),
        Err(CodexAuthError::NotJsonObject),
    );
    assert_eq!(
        validate_codex_auth("\"just-a-string\""),
        Err(CodexAuthError::NotJsonObject),
    );
    // An API key file has no tokens object.
    assert_eq!(
        validate_codex_auth(r#"{"OPENAI_API_KEY":"sk-proj-abc"}"#),
        Err(CodexAuthError::AccessToken),
    );
    // A refresh token but no access token.
    assert_eq!(
        validate_codex_auth(r#"{"tokens":{"refresh_token":"r"}}"#),
        Err(CodexAuthError::AccessToken),
    );
    // An access token but no refresh token, as a Business access-token file.
    assert_eq!(
        validate_codex_auth(r#"{"tokens":{"access_token":"a"}}"#),
        Err(CodexAuthError::RefreshToken),
    );
    // A non-string token is refused, not accepted on presence alone.
    assert_eq!(
        validate_codex_auth(r#"{"tokens":{"access_token":"a","refresh_token":42}}"#),
        Err(CodexAuthError::RefreshToken),
    );
}

#[test]
fn a_codex_auth_error_never_echoes_the_input() {
    // The message names the missing field and carries no part of the input, so
    // the settings page can show it without leaking a token.
    let secret = "sk-proj-super-secret-value";
    let message = validate_codex_auth(&format!(r#"{{"OPENAI_API_KEY":"{secret}"}}"#))
        .expect_err("refused")
        .to_string();
    assert!(
        !message.contains(secret),
        "the error must not echo the input: {message:?}",
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn copilot_token_round_trips() {
    let secrets = LocalSecrets::new(&key(1), store());

    assert!(secrets.copilot_token().expect("read empty").is_none());

    secrets.set_copilot_token("gho_first").expect("set");
    assert_eq!(
        secrets.copilot_token().expect("read stored").as_deref(),
        Some("gho_first"),
    );

    secrets.set_copilot_token("gho_second").expect("replace");
    assert_eq!(
        secrets.copilot_token().expect("read replaced").as_deref(),
        Some("gho_second"),
    );

    secrets.clear_copilot_token().expect("clear");
    assert!(secrets.copilot_token().expect("read cleared").is_none());

    secrets.clear_copilot_token().expect("clear again");
}

#[test]
fn local_key_round_trips_and_clears_without_the_master_key() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));

    assert!(secrets.local_key().expect("read empty").is_none());
    secrets.set_local_key("sk-first").expect("set");
    secrets.set_local_key("sk-second").expect("replace");
    assert_eq!(
        secrets.local_key().expect("read").as_deref(),
        Some("sk-second"),
    );
    let (_, raw) = store
        .get_secret(at("", "", "local-key"))
        .expect("raw row")
        .expect("row exists");
    assert!(!contains(&raw, b"sk-second"));

    let unavailable = UnavailableSecrets::new(Arc::clone(&store));
    assert!(matches!(
        unavailable.local_key(),
        Err(SecretError::Unavailable)
    ));
    assert!(matches!(
        unavailable.set_local_key("sk-x"),
        Err(SecretError::Unavailable),
    ));
    unavailable.clear_local_key().expect("clear");
    assert!(secrets.local_key().expect("read cleared").is_none());
}

#[test]
fn copilot_token_row_holds_no_plaintext() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    secrets
        .set_copilot_token("gho_plaintext_marker")
        .expect("set");

    let (backend, bytes) = store
        .get_secret(at("", "", "copilot-token"))
        .expect("raw row")
        .expect("row exists");
    assert_eq!(backend, LOCAL_BACKEND);
    assert!(
        !bytes
            .windows("gho_plaintext_marker".len())
            .any(|window| window == b"gho_plaintext_marker"),
        "the stored bytes must not contain the token",
    );
}

#[test]
fn deleting_an_mcp_server_named_copilot_keeps_the_token() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    secrets.set_copilot_token("gho_keep").expect("set token");
    secrets
        .put(at("maestro", "copilot", "Authorization"), "Bearer x")
        .expect("put header");

    secrets
        .delete(at("maestro", "copilot", "Authorization"))
        .expect("delete header");
    store.delete_mcp_server("copilot").expect("delete server");

    assert_eq!(
        secrets.copilot_token().expect("read token").as_deref(),
        Some("gho_keep"),
    );
}

#[test]
fn an_unavailable_store_reports_the_copilot_token_unavailable() {
    let store = store();
    let secrets = UnavailableSecrets::new(Arc::clone(&store));
    assert!(matches!(
        secrets.copilot_token(),
        Err(SecretError::Unavailable)
    ));
    assert!(matches!(
        secrets.set_copilot_token("gho_x"),
        Err(SecretError::Unavailable)
    ));
    secrets.clear_copilot_token().expect("clear");
}

#[test]
fn an_unavailable_store_still_clears_a_stored_copilot_token() {
    let store = store();
    LocalSecrets::new(&key(1), Arc::clone(&store))
        .set_copilot_token("gho_x")
        .expect("set");
    UnavailableSecrets::new(Arc::clone(&store))
        .clear_copilot_token()
        .expect("clear");
    assert!(
        store
            .get_secret(at("", "", "copilot-token"))
            .expect("raw row")
            .is_none()
    );
}

#[test]
fn validate_copilot_token_accepts_each_supported_prefix_and_trims() {
    assert_eq!(validate_copilot_token("gho_abc123"), Ok("gho_abc123"));
    assert_eq!(validate_copilot_token("ghu_abc123"), Ok("ghu_abc123"));
    assert_eq!(
        validate_copilot_token("github_pat_11ABC_xyz"),
        Ok("github_pat_11ABC_xyz"),
    );
    assert_eq!(validate_copilot_token("  gho_abc123\n"), Ok("gho_abc123"));
}

#[test]
fn validate_copilot_token_refuses_classic_unknown_and_empty() {
    let classic = validate_copilot_token("ghp_secretclassic").expect_err("classic");
    assert_eq!(classic, CopilotTokenError::Classic);
    assert_eq!(
        classic.to_string(),
        "classic personal access tokens are not supported; use a fine-grained token with the Copilot Requests permission",
    );

    for input in ["sk-ant-oat01-abc", "gho_", "github_pat_", "xgho_abc", "a b"] {
        assert_eq!(
            validate_copilot_token(input),
            Err(CopilotTokenError::UnknownFormat),
            "{input:?}",
        );
    }
    assert_eq!(
        validate_copilot_token("  \n"),
        Err(CopilotTokenError::Empty)
    );
}

#[test]
fn validate_copilot_token_explains_a_prefix_without_a_value() {
    let err = validate_copilot_token("gho_").expect_err("prefix only");
    assert_eq!(
        err.to_string(),
        "the Copilot token must start with gho_, ghu_ or github_pat_ and have a value after the prefix",
    );
}

#[test]
fn validate_copilot_token_errors_never_echo_the_input() {
    for input in ["ghp_secretclassic", "sk-ant-secretvalue", "secretjunk"] {
        let err = validate_copilot_token(input).expect_err("refused");
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("secret"), "{shown}");
    }
}

const GITHUB: ForgeKind = ForgeKind::GitHub;

/// The raw row a forge token owner's ciphertext sits in.
fn forge_row(owner: ForgeTokenOwner<'_>) -> (String, String) {
    match owner {
        ForgeTokenOwner::User { login, forge } => (
            String::new(),
            format!("forge-user:{}:{login}", forge.as_str()),
        ),
        ForgeTokenOwner::Project { project, .. } => (project.into(), "forge-project".into()),
        ForgeTokenOwner::Upstream { project, .. } => (project.into(), "forge-upstream".into()),
    }
}

/// Moves the ciphertext of `from` into the row of `to` and returns what
/// reading `to` gives.
fn move_forge_token(
    store: &Store,
    secrets: &LocalSecrets,
    from: ForgeTokenOwner<'_>,
    to: ForgeTokenOwner<'_>,
) -> Result<Option<String>, SecretError> {
    let (project, header) = forge_row(from);
    let (backend, bytes) = store
        .get_secret(at(&project, "", &header))
        .expect("raw row")
        .expect("row exists");
    let (project, header) = forge_row(to);
    store
        .put_secret(at(&project, "", &header), &backend, &bytes)
        .expect("plant the bytes");
    secrets.forge_token(to)
}

#[test]
fn forge_tokens_round_trip_per_owner_and_hold_no_plaintext() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let alice = ForgeTokenOwner::User {
        login: "alice",
        forge: &GITHUB,
    };
    let bob = ForgeTokenOwner::User {
        login: "bob",
        forge: &GITHUB,
    };
    let project = ForgeTokenOwner::Project {
        project: "maestro",
        forge: &GITHUB,
    };
    let upstream = ForgeTokenOwner::Upstream {
        project: "maestro",
        forge: &GITHUB,
    };

    for (owner, token) in [
        (alice, "tok-alice"),
        (bob, "tok-bob"),
        (project, "tok-project"),
        (upstream, "tok-upstream"),
    ] {
        assert!(secrets.forge_token(owner).unwrap().is_none());
        secrets.put_forge_token(owner, token, &account()).unwrap();
    }
    for (owner, token) in [
        (alice, "tok-alice"),
        (bob, "tok-bob"),
        (project, "tok-project"),
        (upstream, "tok-upstream"),
    ] {
        assert_eq!(secrets.forge_token(owner).unwrap().as_deref(), Some(token));
        let (row_project, header) = forge_row(owner);
        let (_, bytes) = store
            .get_secret(at(&row_project, "", &header))
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(token));
    }

    secrets.delete_forge_token(alice).unwrap();
    assert!(secrets.forge_token(alice).unwrap().is_none());
    assert_eq!(
        secrets.forge_token(bob).unwrap().as_deref(),
        Some("tok-bob")
    );
    secrets.delete_forge_token(alice).unwrap();
}

#[test]
fn a_forge_token_ciphertext_moved_to_another_row_fails_to_decrypt() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let alice = ForgeTokenOwner::User {
        login: "alice",
        forge: &GITHUB,
    };
    let bob = ForgeTokenOwner::User {
        login: "bob",
        forge: &GITHUB,
    };
    let project = ForgeTokenOwner::Project {
        project: "maestro",
        forge: &GITHUB,
    };
    let upstream = ForgeTokenOwner::Upstream {
        project: "maestro",
        forge: &GITHUB,
    };
    for (from, to) in [
        (alice, bob),
        (alice, project),
        (project, upstream),
        (upstream, project),
        (project, alice),
    ] {
        for owner in [alice, project, upstream] {
            secrets.put_forge_token(owner, "tok", &account()).unwrap();
        }
        assert!(
            matches!(
                move_forge_token(&store, &secrets, from, to),
                Err(SecretError::Decrypt)
            ),
            "{from:?} into {to:?}",
        );
    }
}

#[test]
fn a_forge_token_ciphertext_swapped_with_an_mcp_header_or_reserved_row_fails() {
    let store = store();
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let alice = ForgeTokenOwner::User {
        login: "alice",
        forge: &GITHUB,
    };
    let header = at("maestro", "context7", "Authorization");
    secrets
        .put_forge_token(alice, "tok-alice", &account())
        .unwrap();
    secrets.put(header, "Bearer sk-header").unwrap();
    secrets.set_claude_token("sk-ant-oat01-token").unwrap();
    let (_, header_header) = forge_row(alice);

    // The forge token's ciphertext in an MCP header row and in the Claude row.
    let (backend, bytes) = store
        .get_secret(at("", "", &header_header))
        .unwrap()
        .unwrap();
    store.put_secret(header, &backend, &bytes).unwrap();
    assert!(matches!(secrets.get(header), Err(SecretError::Decrypt)));
    store
        .put_secret(at("", "", "claude-token"), &backend, &bytes)
        .unwrap();
    assert!(matches!(secrets.claude_token(), Err(SecretError::Decrypt)));

    // An MCP header ciphertext in the forge token row.
    secrets.put(header, "Bearer sk-header").unwrap();
    let (backend, bytes) = store.get_secret(header).unwrap().unwrap();
    store
        .put_secret(at("", "", &header_header), &backend, &bytes)
        .unwrap();
    assert!(matches!(
        secrets.forge_token(alice),
        Err(SecretError::Decrypt)
    ));
}

#[test]
fn an_unavailable_store_refuses_forge_tokens_but_still_deletes_them() {
    let store = store();
    let local = LocalSecrets::new(&key(1), Arc::clone(&store));
    let alice = ForgeTokenOwner::User {
        login: "alice",
        forge: &GITHUB,
    };
    local.put_forge_token(alice, "tok", &account()).unwrap();

    let secrets = UnavailableSecrets::new(Arc::clone(&store));
    assert!(matches!(
        secrets.put_forge_token(alice, "tok", &account()),
        Err(SecretError::Unavailable)
    ));
    assert!(matches!(
        secrets.forge_token(alice),
        Err(SecretError::Unavailable)
    ));
    secrets.delete_forge_token(alice).unwrap();
    assert!(local.forge_token(alice).unwrap().is_none());
}

#[test]
fn a_failed_account_write_rolls_the_forge_token_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    let secrets = LocalSecrets::new(&key(1), Arc::clone(&store));
    let alice = ForgeTokenOwner::User {
        login: "alice",
        forge: &GITHUB,
    };
    secrets
        .put_forge_token(alice, "tok-old", &account())
        .unwrap();

    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .execute_batch(
            "CREATE TRIGGER no_account_write BEFORE UPDATE ON user_forge_accounts
             BEGIN SELECT RAISE(ABORT, 'refused'); END;
             CREATE TRIGGER no_account_erase BEFORE DELETE ON user_forge_accounts
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

    assert!(
        secrets
            .put_forge_token(alice, "tok-new", &account())
            .is_err()
    );
    assert_eq!(
        secrets.forge_token(alice).unwrap().as_deref(),
        Some("tok-old")
    );
    assert!(secrets.delete_forge_token(alice).is_err());
    assert_eq!(
        secrets.forge_token(alice).unwrap().as_deref(),
        Some("tok-old")
    );
    assert_eq!(store.forge_account(alice).unwrap(), Some(account()));
}
