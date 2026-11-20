//! Tests for the encrypted secret store. No network and no files: the store
//! runs in memory and the cipher is deterministic given its key.

use std::sync::Arc;

use factory_server::{
    LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN, SecretError, SecretRef, SecretStore, Store,
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
