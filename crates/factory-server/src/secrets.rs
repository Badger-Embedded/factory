//! Encrypted storage for per-project MCP header values.
//!
//! A header value for an MCP server is a credential, so it must not sit in
//! SQLite as plain text. Access goes through the [`SecretStore`] trait, so a
//! later external backend such as Vault can replace or sit beside the local
//! one. [`LocalSecrets`] is the only backend for now: it encrypts each value
//! with XChaCha20-Poly1305 under a master key that lives outside the database,
//! and stores the ciphertext in the `secrets` table through [`Store`]. Each
//! row records its backend, so an external backend can be added without a
//! schema change for existing rows.

use std::fmt;
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::store::{Store, StoreError};

/// The master key length in bytes, which is also the XChaCha20-Poly1305 key
/// length.
pub const MASTER_KEY_LEN: usize = 32;

/// The XChaCha20-Poly1305 nonce length in bytes. Each value gets a fresh
/// random nonce, stored in front of its ciphertext.
const NONCE_LEN: usize = 24;

/// The `backend` value a [`LocalSecrets`] row carries. A future external
/// backend records its own name, so the dispatch reads the column rather than
/// assuming one backend, and old rows survive a new backend unchanged.
pub const LOCAL_BACKEND: &str = "local";

/// Where a secret lives: the project that owns it, the MCP server it is for,
/// and the header name it fills. The three together are the primary key of
/// the `secrets` table and the associated data bound into each ciphertext.
#[derive(Debug, Clone, Copy)]
pub struct SecretRef<'a> {
    pub project: &'a str,
    pub server: &'a str,
    pub header: &'a str,
}

/// A failure reading or writing a secret. None of these variants carry a
/// plaintext value, so a logged or displayed error never leaks a credential.
#[derive(Debug)]
pub enum SecretError {
    /// No usable master key, so values can be neither read nor written. The
    /// settings page reports this and refuses to save a header value.
    Unavailable,
    /// A stored value did not decrypt: the key is wrong, the row was tampered
    /// with, or a ciphertext was copied from another project, server or
    /// header. A run treats this like an unreachable server and skips it.
    Decrypt,
    /// Encryption failed, which the cipher does not do for a valid key and an
    /// in-memory buffer. Kept so `put` never panics.
    Encrypt,
    /// The row belongs to a backend this build does not know.
    UnknownBackend(String),
    Store(StoreError),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Unavailable => write!(f, "the secret store is unavailable"),
            SecretError::Decrypt => write!(f, "a stored secret did not decrypt"),
            SecretError::Encrypt => write!(f, "a secret could not be encrypted"),
            SecretError::UnknownBackend(backend) => {
                write!(f, "unknown secret backend {backend:?}")
            }
            SecretError::Store(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SecretError {}

impl From<StoreError> for SecretError {
    fn from(err: StoreError) -> Self {
        SecretError::Store(err)
    }
}

/// Stores a credential for a project, server and header, keyed by all three.
/// Put, get and delete are the whole surface; the dashboard and the proxy use
/// a `&dyn SecretStore`, so the backend can change without touching them.
pub trait SecretStore: Send + Sync {
    /// Stores `value` for `at`, replacing any value already there.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn put(&self, at: SecretRef<'_>, value: &str) -> Result<(), SecretError>;

    /// The value stored for `at`, or `None` when none is.
    ///
    /// # Errors
    ///
    /// Fails when the stored value does not decrypt, when it belongs to an
    /// unknown backend, or on a storage error.
    fn get(&self, at: SecretRef<'_>) -> Result<Option<String>, SecretError>;

    /// Deletes the value stored for `at`. Deleting a value that is not there
    /// is fine.
    ///
    /// # Errors
    ///
    /// Fails on a storage error.
    fn delete(&self, at: SecretRef<'_>) -> Result<(), SecretError>;
}

/// The local encrypted backend. It holds the cipher built from the master key
/// and writes ciphertext into the `secrets` table through [`Store`]. The key
/// never touches the database, so a stolen database file leaks nothing.
pub struct LocalSecrets {
    cipher: XChaCha20Poly1305,
    store: Arc<Store>,
}

impl LocalSecrets {
    /// Builds the backend from a 32-byte master `key` and the store that holds
    /// the `secrets` table.
    #[must_use]
    pub fn new(key: &[u8; MASTER_KEY_LEN], store: Arc<Store>) -> Self {
        LocalSecrets {
            cipher: XChaCha20Poly1305::new(Key::from_slice(key)),
            store,
        }
    }
}

impl SecretStore for LocalSecrets {
    fn put(&self, at: SecretRef<'_>, value: &str) -> Result<(), SecretError> {
        let aad = associated_data(at);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::Encrypt)?;
        let mut stored = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        stored.extend_from_slice(nonce.as_slice());
        stored.extend_from_slice(&ciphertext);
        self.store.put_secret(at, LOCAL_BACKEND, &stored)?;
        Ok(())
    }

    fn get(&self, at: SecretRef<'_>) -> Result<Option<String>, SecretError> {
        let Some((backend, stored)) = self.store.get_secret(at)? else {
            return Ok(None);
        };
        if backend != LOCAL_BACKEND {
            return Err(SecretError::UnknownBackend(backend));
        }
        if stored.len() < NONCE_LEN {
            return Err(SecretError::Decrypt);
        }
        let (nonce, ciphertext) = stored.split_at(NONCE_LEN);
        let aad = associated_data(at);
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::Decrypt)?;
        let value = String::from_utf8(plaintext).map_err(|_| SecretError::Decrypt)?;
        Ok(Some(value))
    }

    fn delete(&self, at: SecretRef<'_>) -> Result<(), SecretError> {
        self.store.delete_secret(at)?;
        Ok(())
    }
}

/// The associated data bound into a ciphertext: the project, server and
/// header, each prefixed with its length so no two different triples ever
/// build the same bytes. A value copied into another row then fails to
/// decrypt, because its associated data no longer matches.
fn associated_data(at: SecretRef<'_>) -> Vec<u8> {
    let mut aad = Vec::new();
    for part in [at.project, at.server, at.header] {
        aad.extend_from_slice(&(part.len() as u64).to_le_bytes());
        aad.extend_from_slice(part.as_bytes());
    }
    aad
}

/// The master key from `FACTORY_MASTER_KEY`, the `factory-master-key` podman
/// secret, as 64 hex characters. `None` when the variable is unset, empty, not
/// hex, or not 32 bytes, so the factory starts and reports secrets unavailable
/// rather than refusing to run.
#[must_use]
pub fn master_key_from_env() -> Option<[u8; MASTER_KEY_LEN]> {
    decode_key(std::env::var("FACTORY_MASTER_KEY").ok()?.trim())
}

/// Decodes a 64-character hex string into a 32-byte key, or `None` when it is
/// the wrong length or not hex.
fn decode_key(hex: &str) -> Option<[u8; MASTER_KEY_LEN]> {
    if hex.len() != MASTER_KEY_LEN * 2 {
        return None;
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}
