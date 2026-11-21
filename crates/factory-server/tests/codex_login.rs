//! Tests for the Codex login coordinator: resolving the stored login for a run,
//! keeping a newer refreshed login the worker read back, and serialising runs
//! while the stored login is due for a refresh. The store runs in memory and no
//! worker runs: the coordinator's compare, store and gate are exercised on their
//! own.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use factory_server::{CodexLogin, LocalSecrets, MASTER_KEY_LEN, SecretStore, Store};
use tokio::time::timeout;

/// 2026-11-21T00:00:00Z, the wall clock the due checks run against.
const NOW: u64 = 1_795_219_200;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

fn secrets() -> Arc<dyn SecretStore> {
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], store))
}

/// A subscription `auth.json` with the given `last_refresh` and an opaque,
/// non-JWT access token, so its freshness rests on `last_refresh` alone.
fn auth(last_refresh: &str) -> String {
    format!(
        r#"{{"tokens":{{"access_token":"at-{last_refresh}","refresh_token":"rt"}},"last_refresh":"{last_refresh}"}}"#
    )
}

/// A login whose access token is a JWT that expires `in_secs` from [`NOW`], and
/// whose `last_refresh` is recent, so only the expiry can make it due.
fn auth_expiring_in(in_secs: i64) -> String {
    let exp = i64::try_from(NOW).unwrap() + in_secs;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    let token = format!("header.{payload}.signature");
    format!(
        r#"{{"tokens":{{"access_token":"{token}","refresh_token":"rt"}},"last_refresh":"2026-11-20T00:00:00Z"}}"#
    )
}

#[test]
fn credential_resolves_the_stored_login_or_fails() {
    let secrets = secrets();
    let codex = CodexLogin::new(Arc::clone(&secrets));
    assert_eq!(codex.credential(), Err("no Codex login is set".to_owned()));

    let login = auth("2026-11-20T00:00:00Z");
    secrets.set_codex_auth(&login).unwrap();
    assert_eq!(codex.credential(), Ok(login));
}

#[tokio::test]
async fn a_newer_refreshed_login_is_stored() {
    let secrets = secrets();
    let stored = auth("2026-11-20T00:00:00Z");
    secrets.set_codex_auth(&stored).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    // The worker read back a file Codex refreshed to a newer last_refresh.
    let refreshed = auth("2026-11-28T00:00:00Z");
    codex.store_refreshed(&refreshed).await;

    assert_eq!(
        secrets.codex_auth().unwrap().as_deref(),
        Some(refreshed.as_str())
    );
}

#[tokio::test]
async fn a_login_not_newer_than_the_stored_one_is_kept() {
    let secrets = secrets();
    let stored = auth("2026-11-28T00:00:00Z");
    secrets.set_codex_auth(&stored).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    // An older read-back file, and the same file again, both leave the store as
    // it was.
    codex.store_refreshed(&auth("2026-11-20T00:00:00Z")).await;
    codex.store_refreshed(&stored).await;

    assert_eq!(
        secrets.codex_auth().unwrap().as_deref(),
        Some(stored.as_str())
    );
}

#[tokio::test]
async fn an_invalid_read_back_file_is_dropped() {
    let secrets = secrets();
    let stored = auth("2026-11-20T00:00:00Z");
    secrets.set_codex_auth(&stored).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    // A file that is not a subscription login, even with a newer-looking
    // timestamp, never replaces the stored login.
    codex
        .store_refreshed(r#"{"OPENAI_API_KEY":"sk","last_refresh":"2027-01-01T00:00:00Z"}"#)
        .await;

    assert_eq!(
        secrets.codex_auth().unwrap().as_deref(),
        Some(stored.as_str())
    );
}

#[tokio::test]
async fn a_due_login_serialises_two_runs() {
    let secrets = secrets();
    // Older than seven days, so the stored login is due for a refresh.
    let login = auth("2026-11-01T00:00:00Z");
    secrets.set_codex_auth(&login).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    // The first run holds the refresh gate.
    let first = codex.enter_run(login.clone(), now()).await;

    // A second run while the first holds the gate cannot start.
    let second = codex.enter_run(login.clone(), now());
    tokio::pin!(second);
    assert!(
        timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "the second run should wait while the login is due",
    );

    // Once the first run ends, the second acquires the gate and starts.
    drop(first);
    assert!(
        timeout(Duration::from_millis(500), &mut second)
            .await
            .is_ok(),
        "the second run should start once the first ends",
    );
}

#[tokio::test]
async fn a_login_not_due_lets_runs_start_at_once() {
    let secrets = secrets();
    // One day old, so the stored login is not due.
    let login = auth("2026-11-20T00:00:00Z");
    secrets.set_codex_auth(&login).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    let _first = codex.enter_run(login.clone(), now()).await;
    let second = codex.enter_run(login.clone(), now());
    tokio::pin!(second);
    assert!(
        timeout(Duration::from_millis(100), &mut second)
            .await
            .is_ok(),
        "a login that is not due puts no limit on concurrent runs",
    );
}

#[tokio::test]
async fn a_login_whose_token_expires_soon_is_due() {
    let secrets = secrets();
    // Recent last_refresh, so only the near expiry can make it due.
    let login = auth_expiring_in(30 * 60);
    secrets.set_codex_auth(&login).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    let _first = codex.enter_run(login.clone(), now()).await;
    let second = codex.enter_run(login.clone(), now());
    tokio::pin!(second);
    assert!(
        timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "a login expiring within the hour is due, so runs serialise",
    );
}

#[tokio::test]
async fn a_login_whose_token_expires_later_is_not_due() {
    let secrets = secrets();
    let login = auth_expiring_in(2 * 60 * 60);
    secrets.set_codex_auth(&login).unwrap();
    let codex = CodexLogin::new(Arc::clone(&secrets));

    let _first = codex.enter_run(login.clone(), now()).await;
    let second = codex.enter_run(login.clone(), now());
    tokio::pin!(second);
    assert!(
        timeout(Duration::from_millis(100), &mut second)
            .await
            .is_ok(),
        "a token with over an hour left and a recent refresh is not due",
    );
}
