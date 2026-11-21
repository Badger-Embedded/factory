//! The factory's Codex subscription login: resolving it for a run and keeping
//! the file Codex rewrites when it refreshes the tokens.
//!
//! Codex reads its `auth.json` from `CODEX_HOME` and writes a new one back
//! whenever it refreshes the access token, about every eight days or near
//! expiry. The factory keeps the login in the secret store, places it in the
//! run's config directory before the worker starts and reads it back
//! afterwards; the podman executor does the placing and reading, and
//! [`CodexLogin`] does the rest:
//!
//! - It resolves the stored login for a run, failing with "no Codex login is
//!   set" when there is none, the way the Claude token fails.
//! - It decides when a stored login is due for a refresh, by its `last_refresh`
//!   being older than [`REFRESH_AGE`] or its access token expiring within
//!   [`EXPIRY_MARGIN`]. While a login is due it serialises Codex runs through
//!   one gate, so two runs cannot rotate the same refresh token at once.
//! - It stores a refreshed file a run read back, under a single lock so two
//!   runs that end together cannot write over each other, and only when the
//!   file is valid and its `last_refresh` is newer than the stored login's.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};

use crate::secrets::{SecretStore, validate_codex_auth};

/// A stored login older than this by its `last_refresh` is due for a refresh.
const REFRESH_AGE: Duration = Duration::from_hours(7 * 24);

/// A stored login whose access token expires within this is due for a refresh.
const EXPIRY_MARGIN: Duration = Duration::from_hours(1);

/// The account-wide Codex login, read for a run and kept across the refreshes
/// Codex writes mid-run. One instance is shared by the registry, which resolves
/// the credential, and the executor, which serialises due runs and stores the
/// refreshed file.
pub struct CodexLogin {
    secrets: Arc<dyn SecretStore>,
    /// Held for a whole run while the stored login is due for a refresh, so at
    /// most one such run is in flight and two runs cannot rotate the same
    /// refresh token at once.
    refresh_gate: Mutex<()>,
    /// Held across the compare-and-store after a run, so two runs that end
    /// together cannot write over each other's refreshed login.
    store_lock: Mutex<()>,
}

/// A Codex run in progress, from [`CodexLogin::enter_run`]. It carries the login
/// to write into the config directory and, while the stored login is due for a
/// refresh, the gate guard that keeps a second Codex run waiting until this one
/// ends. The executor holds it from before the worker starts until after the
/// refreshed login is stored.
pub struct CodexRun<'a> {
    /// The login to write into the run's config directory.
    pub auth: String,
    /// The refresh gate, held while the login was due. `None` when the login
    /// was not due, so runs do not serialise.
    _gate: Option<MutexGuard<'a, ()>>,
}

impl CodexLogin {
    /// Builds the coordinator from the secret store it reads and writes the
    /// login through.
    #[must_use]
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        CodexLogin {
            secrets,
            refresh_gate: Mutex::new(()),
            store_lock: Mutex::new(()),
        }
    }

    /// The stored login for a run, read afresh on each call, or the reason a run
    /// cannot start. With no login stored, or when the store is unavailable or
    /// the value will not decrypt, the reason is "no Codex login is set", so a
    /// run fails before any checkout or container the way a Claude run does with
    /// "no Claude token is set".
    ///
    /// # Errors
    ///
    /// Returns "no Codex login is set" when no usable login is stored.
    pub fn credential(&self) -> Result<String, String> {
        match self.secrets.codex_auth() {
            Ok(Some(auth)) => Ok(auth),
            Ok(None) => Err("no Codex login is set".to_owned()),
            Err(err) => {
                tracing::warn!("reading the stored Codex login failed: {err}");
                Err("no Codex login is set".to_owned())
            }
        }
    }

    /// Enters a Codex run: the login to write into the config directory and,
    /// while the stored login is due for a refresh, a guard that serialises this
    /// run against other Codex runs. `resolved` is the login the caller already
    /// read. When the login is not due, the run starts at once with no gate.
    /// When it is due, the run waits for the gate, then re-reads the login,
    /// because a run that held the gate may have refreshed it.
    pub async fn enter_run(&self, resolved: String, now: SystemTime) -> CodexRun<'_> {
        if !is_due(&resolved, now) {
            return CodexRun {
                auth: resolved,
                _gate: None,
            };
        }
        let gate = self.refresh_gate.lock().await;
        let auth = match self.secrets.codex_auth() {
            Ok(Some(auth)) => auth,
            _ => resolved,
        };
        CodexRun {
            auth,
            _gate: Some(gate),
        }
    }

    /// Stores the login a run read back after the worker exited, when it is a
    /// valid subscription login and its `last_refresh` is newer than the stored
    /// login's. The compare and the store happen under one lock, so two runs
    /// that end together cannot write over each other. A file that is invalid,
    /// the same as the stored one, or older is dropped and the stored login is
    /// kept.
    pub async fn store_refreshed(&self, refreshed: &str) {
        if validate_codex_auth(refreshed).is_err() {
            return;
        }
        let _lock = self.store_lock.lock().await;
        let stored = match self.secrets.codex_auth() {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!("reading the stored Codex login failed: {err}");
                return;
            }
        };
        let newer = stored
            .as_deref()
            .is_none_or(|stored| is_newer(refreshed, stored));
        if newer && let Err(err) = self.secrets.set_codex_auth(refreshed) {
            tracing::warn!("storing the refreshed Codex login failed: {err}");
        }
    }
}

/// Whether a stored login should be refreshed before another run uses it: its
/// `last_refresh` is older than [`REFRESH_AGE`], or its access token expires
/// within [`EXPIRY_MARGIN`]. A login the factory cannot read a `last_refresh`
/// from is treated as due, so an unexpected shape serialises rather than lets
/// two runs rotate at once.
fn is_due(auth: &str, now: SystemTime) -> bool {
    let old = match last_refresh(auth) {
        Some(at) => now.duration_since(at).is_ok_and(|age| age > REFRESH_AGE),
        None => true,
    };
    let expiring = access_token_exp(auth).is_some_and(|exp| exp <= now + EXPIRY_MARGIN);
    old || expiring
}

/// Whether `new` is a newer login than `old`, by `last_refresh`. A `new` with a
/// `last_refresh` and an `old` without one counts as newer; anything else that
/// cannot be compared counts as not newer, so the stored login is kept.
fn is_newer(new: &str, old: &str) -> bool {
    match (last_refresh(new), last_refresh(old)) {
        (Some(new), Some(old)) => new > old,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The `last_refresh` time of a login, or `None` when it is missing or not a
/// time the factory can parse.
fn last_refresh(auth: &str) -> Option<SystemTime> {
    let value: Value = serde_json::from_str(auth).ok()?;
    let text = value.get("last_refresh")?.as_str()?;
    parse_rfc3339(text)
}

/// The expiry time in the login's access token, read from the `exp` claim of
/// the JWT payload without verifying the signature, or `None` when the token is
/// not a JWT or carries no numeric `exp`.
fn access_token_exp(auth: &str) -> Option<SystemTime> {
    let value: Value = serde_json::from_str(auth).ok()?;
    let token = value.pointer("/tokens/access_token")?.as_str()?;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = claims.get("exp")?.as_i64()?;
    let exp = u64::try_from(exp).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(exp))
}

/// Parses an RFC 3339 time, such as the `2026-11-20T00:00:00Z` Codex writes for
/// `last_refresh`, to a [`SystemTime`] at second resolution. It accepts a `Z`
/// suffix or a numeric offset and ignores any fractional seconds. `None` when
/// the string is not that shape or is before the Unix epoch.
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    let field = |range: std::ops::Range<usize>| text.get(range).and_then(|s| s.parse::<i64>().ok());
    let year = field(0..4)?;
    let month = field(5..7)?;
    let day = field(8..10)?;
    let hour = field(11..13)?;
    let minute = field(14..16)?;
    let second = field(17..19)?;
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let rest = &text[19..];
    // Skip a fractional-seconds part, such as `.123456`.
    let rest = match rest.strip_prefix('.') {
        Some(frac) => {
            let end = frac
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(frac.len());
            &frac[end..]
        }
        None => rest,
    };
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let offset_hour = rest.get(1..3)?.parse::<i64>().ok()?;
            let offset_minute = if rest.as_bytes().get(3) == Some(&b':') {
                rest.get(4..6)?.parse::<i64>().ok()?
            } else {
                rest.get(3..5).map_or(Some(0), |m| m.parse::<i64>().ok())?
            };
            let magnitude = offset_hour * 3600 + offset_minute * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second - offset;
    let seconds = u64::try_from(seconds).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Days from the Unix epoch to a proleptic-Gregorian date, by Howard Hinnant's
/// `days_from_civil`. It is exact for every year, so a `last_refresh` compares
/// and ages correctly across month and year boundaries.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
