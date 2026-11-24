//! Encrypted storage for the factory's at-rest credentials: per-project MCP
//! header values, the account-wide Claude token, the account-wide Codex login,
//! the account-wide Copilot token and the forge tokens (a login's own, a
//! project's and a packaging upstream's).
//!
//! A header value for an MCP server is a credential, so it must not sit in
//! SQLite as plain text. The Claude subscription token from `claude
//! setup-token` is the same, and so is the Codex `auth.json` a `ChatGPT` Plus or
//! Pro login writes: both are stored here instead of a podman secret, so an
//! operator can set them from the settings page. Access goes through the
//! [`SecretStore`] trait, so a later external backend such as Vault can replace
//! or sit beside the local one. [`LocalSecrets`] is the only backend for now:
//! it encrypts each value with XChaCha20-Poly1305 under a master key that lives
//! outside the database, and stores the ciphertext in the `secrets` table
//! through [`Store`]. Each row records its backend, so an external backend can
//! be added without a schema change for existing rows.
//!
//! The Claude token, the Codex login and the Copilot token each live in one
//! reserved row keyed apart from every project/server/header credential and
//! from each other, with its own associated data, so deleting or renaming an
//! MCP server never touches any of them and a ciphertext moved between the
//! three rows and an MCP header row fails to decrypt.
//!
//! A forge token lives in a row of its own per owner, see [`ForgeTokenOwner`].
//! Its associated data binds the owner kind, the key and the forge, so a
//! ciphertext moved to another owner's row, to an MCP header row or to a
//! reserved row fails to decrypt.

use std::fmt;
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use factory_runner::{ForgeAccount, ForgeKind};

use crate::store::{ProjectGuard, Store, StoreError};

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

/// Whose forge token a row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeTokenOwner<'a> {
    /// The token a login stores for itself on the forge's settings page.
    User {
        login: &'a str,
        forge: &'a ForgeKind,
    },
    /// The token a project's jobs run as when no personal account applies.
    Project {
        project: &'a str,
        forge: &'a ForgeKind,
    },
    /// The optional token that reads a packaging upstream.
    Upstream {
        project: &'a str,
        forge: &'a ForgeKind,
    },
}

impl<'a> ForgeTokenOwner<'a> {
    /// The owner kind, the key and the forge, the three parts the associated
    /// data binds.
    fn parts(&self) -> (&'static str, &'a str, &'a str) {
        match *self {
            ForgeTokenOwner::User { login, forge } => ("user", login, forge.as_str()),
            ForgeTokenOwner::Project { project, forge } => ("project", project, forge.as_str()),
            ForgeTokenOwner::Upstream { project, forge } => ("upstream", project, forge.as_str()),
        }
    }

    /// The `project` and `header` of the row. The `server` is empty, which no
    /// MCP server name can be, so deleting an MCP server never matches a
    /// forge token row.
    pub(crate) fn row(&self) -> (String, String) {
        match *self {
            ForgeTokenOwner::User { login, forge } => (
                String::new(),
                format!("forge-user:{}:{login}", forge.as_str()),
            ),
            ForgeTokenOwner::Project { project, .. } => {
                (project.to_owned(), "forge-project".into())
            }
            ForgeTokenOwner::Upstream { project, .. } => {
                (project.to_owned(), "forge-upstream".into())
            }
        }
    }

    fn aad(&self) -> Vec<u8> {
        let (kind, key, forge) = self.parts();
        let mut aad = FORGE_TOKEN_AAD.to_vec();
        for part in [kind, key, forge] {
            aad.extend_from_slice(&(part.len() as u64).to_le_bytes());
            aad.extend_from_slice(part.as_bytes());
        }
        aad
    }
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
    /// Whether values can be read and written. [`LocalSecrets`] returns `true`;
    /// [`UnavailableSecrets`], used when no usable master key is present,
    /// returns `false`. The settings page reads this to report the store state
    /// and to refuse saving a header value.
    fn available(&self) -> bool {
        true
    }

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

    /// The stored Claude token, or `None` when none is set. This is the
    /// account-wide subscription token the workers use, kept in a reserved row
    /// apart from the MCP header values.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable, when the stored value does not
    /// decrypt, or on a storage error.
    fn claude_token(&self) -> Result<Option<String>, SecretError>;

    /// Stores `value` as the Claude token, replacing any token already there.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn set_claude_token(&self, value: &str) -> Result<(), SecretError>;

    /// Deletes the stored Claude token. Clearing a token that is not there is
    /// fine.
    ///
    /// # Errors
    ///
    /// Fails on a storage error.
    fn clear_claude_token(&self) -> Result<(), SecretError>;

    /// The stored Codex `auth.json`, or `None` when none is set. This is the
    /// account-wide `ChatGPT` Plus or Pro login the workers use, kept in its own
    /// reserved row apart from both the MCP header values and the Claude token.
    /// The bytes are the file as last written: the caller parses them.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable, when the stored value does not
    /// decrypt, or on a storage error.
    fn codex_auth(&self) -> Result<Option<String>, SecretError>;

    /// Stores `value` as the Codex login, replacing any login already there.
    /// The caller validates it with [`validate_codex_auth`] first; the store
    /// keeps the bytes as given.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn set_codex_auth(&self, value: &str) -> Result<(), SecretError>;

    /// Deletes the stored Codex login. Clearing a login that is not there is
    /// fine.
    ///
    /// # Errors
    ///
    /// Fails on a storage error.
    fn clear_codex_auth(&self) -> Result<(), SecretError>;

    /// The stored Copilot token, or `None` when none is set. This is the
    /// account-wide GitHub token the Copilot workers use, kept in its own
    /// reserved row apart from the MCP header values, the Claude token and the
    /// Codex login.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable, when the stored value does not
    /// decrypt, or on a storage error.
    fn copilot_token(&self) -> Result<Option<String>, SecretError>;

    /// Stores `value` as the Copilot token, replacing any token already there.
    /// The caller validates it with [`crate::validate_copilot_token`] first; the store
    /// keeps the bytes as given.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn set_copilot_token(&self, value: &str) -> Result<(), SecretError>;

    /// Deletes the stored Copilot token. Clearing a token that is not there is
    /// fine.
    ///
    /// # Errors
    ///
    /// Fails on a storage error.
    fn clear_copilot_token(&self) -> Result<(), SecretError>;

    /// Stores `value` as the forge token of `owner`, together with the
    /// `account` it belongs to, in one transaction. It replaces any token
    /// already there.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn put_forge_token(
        &self,
        owner: ForgeTokenOwner<'_>,
        value: &str,
        account: &ForgeAccount,
    ) -> Result<(), SecretError>;

    /// As [`SecretStore::put_forge_token`] for a project token, and stores
    /// nothing unless the project and collaborator in `guard` still exist.
    /// Returns whether it stored.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable or on a storage error.
    fn put_project_forge_token(
        &self,
        owner: ForgeTokenOwner<'_>,
        value: &str,
        account: &ForgeAccount,
        guard: &ProjectGuard<'_>,
    ) -> Result<bool, SecretError>;

    /// The forge token of `owner`, or `None` when none is stored.
    ///
    /// # Errors
    ///
    /// Fails when the store is unavailable, when the stored value does not
    /// decrypt, or on a storage error.
    fn forge_token(&self, owner: ForgeTokenOwner<'_>) -> Result<Option<String>, SecretError>;

    /// The forge token of `owner` with the account recorded for it, read in
    /// one store snapshot. `None` when either is missing.
    ///
    /// # Errors
    ///
    /// As [`SecretStore::forge_token`].
    fn forge_token_with_account(
        &self,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount)>, SecretError>;

    /// As [`SecretStore::forge_token_with_account`], with the sealed bytes of
    /// the row the token came from.
    ///
    /// # Errors
    ///
    /// As [`SecretStore::forge_token`].
    fn forge_token_sealed(
        &self,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount, Vec<u8>)>, SecretError>;

    /// Deletes the forge token of `owner` and its recorded account in one
    /// transaction. Clearing a token that is not there
    /// is fine, and it works with the store unavailable.
    ///
    /// # Errors
    ///
    /// Fails on a storage error.
    fn delete_forge_token(&self, owner: ForgeTokenOwner<'_>) -> Result<(), SecretError>;
}

/// The `secrets` row that holds the account-wide Claude token. Its `server` is
/// empty, which no MCP server name can be (names are 1 to 32 characters of
/// `a-z`, `0-9` and `-`), so `DELETE FROM secrets WHERE server = ?` for any
/// MCP server, including one named `claude`, never matches this row. The
/// `project` and `header` are fixed too, so the token is one well-known key
/// that cannot collide with a project/server/header credential.
const CLAUDE_TOKEN_REF: SecretRef<'static> = SecretRef {
    project: "",
    server: "",
    header: "claude-token",
};

/// The `secrets` row that holds the account-wide Codex `auth.json`. Like
/// [`CLAUDE_TOKEN_REF`] its `server` is empty, which no MCP server name can be,
/// so `DELETE FROM secrets WHERE server = ?` for any MCP server, including one
/// named `codex`, never matches it. Its `header` differs from the Claude
/// token's, so the two reserved rows never collide.
const CODEX_AUTH_REF: SecretRef<'static> = SecretRef {
    project: "",
    server: "",
    header: "codex-auth",
};

const COPILOT_TOKEN_REF: SecretRef<'static> = SecretRef {
    project: "",
    server: "",
    header: "copilot-token",
};

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

    /// Encrypts `value` under `aad` with a fresh nonce and returns the nonce
    /// followed by the ciphertext, the bytes a `secrets` row holds.
    fn seal(&self, aad: &[u8], value: &str) -> Result<Vec<u8>, SecretError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad,
                },
            )
            .map_err(|_| SecretError::Encrypt)?;
        let mut stored = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        stored.extend_from_slice(nonce.as_slice());
        stored.extend_from_slice(&ciphertext);
        Ok(stored)
    }

    /// Decrypts a `secrets` row's bytes under `aad`, after checking the backend
    /// the row records.
    fn open(&self, backend: &str, stored: &[u8], aad: &[u8]) -> Result<String, SecretError> {
        if backend != LOCAL_BACKEND {
            return Err(SecretError::UnknownBackend(backend.to_owned()));
        }
        if stored.len() < NONCE_LEN {
            return Err(SecretError::Decrypt);
        }
        let (nonce, ciphertext) = stored.split_at(NONCE_LEN);
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| SecretError::Decrypt)?;
        String::from_utf8(plaintext).map_err(|_| SecretError::Decrypt)
    }
}

impl SecretStore for LocalSecrets {
    fn put(&self, at: SecretRef<'_>, value: &str) -> Result<(), SecretError> {
        let stored = self.seal(&associated_data(at), value)?;
        self.store.put_secret(at, LOCAL_BACKEND, &stored)?;
        Ok(())
    }

    fn get(&self, at: SecretRef<'_>) -> Result<Option<String>, SecretError> {
        let Some((backend, stored)) = self.store.get_secret(at)? else {
            return Ok(None);
        };
        Ok(Some(self.open(&backend, &stored, &associated_data(at))?))
    }

    fn delete(&self, at: SecretRef<'_>) -> Result<(), SecretError> {
        self.store.delete_secret(at)?;
        Ok(())
    }

    fn claude_token(&self) -> Result<Option<String>, SecretError> {
        let Some((backend, stored)) = self.store.get_secret(CLAUDE_TOKEN_REF)? else {
            return Ok(None);
        };
        Ok(Some(self.open(&backend, &stored, CLAUDE_TOKEN_AAD)?))
    }

    fn set_claude_token(&self, value: &str) -> Result<(), SecretError> {
        let stored = self.seal(CLAUDE_TOKEN_AAD, value)?;
        self.store
            .put_secret(CLAUDE_TOKEN_REF, LOCAL_BACKEND, &stored)?;
        Ok(())
    }

    fn clear_claude_token(&self) -> Result<(), SecretError> {
        self.store.delete_secret(CLAUDE_TOKEN_REF)?;
        Ok(())
    }

    fn codex_auth(&self) -> Result<Option<String>, SecretError> {
        let Some((backend, stored)) = self.store.get_secret(CODEX_AUTH_REF)? else {
            return Ok(None);
        };
        Ok(Some(self.open(&backend, &stored, CODEX_AUTH_AAD)?))
    }

    fn set_codex_auth(&self, value: &str) -> Result<(), SecretError> {
        let stored = self.seal(CODEX_AUTH_AAD, value)?;
        self.store
            .put_secret(CODEX_AUTH_REF, LOCAL_BACKEND, &stored)?;
        Ok(())
    }

    fn clear_codex_auth(&self) -> Result<(), SecretError> {
        self.store.delete_secret(CODEX_AUTH_REF)?;
        Ok(())
    }

    fn copilot_token(&self) -> Result<Option<String>, SecretError> {
        let Some((backend, stored)) = self.store.get_secret(COPILOT_TOKEN_REF)? else {
            return Ok(None);
        };
        Ok(Some(self.open(&backend, &stored, COPILOT_TOKEN_AAD)?))
    }

    fn set_copilot_token(&self, value: &str) -> Result<(), SecretError> {
        let stored = self.seal(COPILOT_TOKEN_AAD, value)?;
        self.store
            .put_secret(COPILOT_TOKEN_REF, LOCAL_BACKEND, &stored)?;
        Ok(())
    }

    fn clear_copilot_token(&self) -> Result<(), SecretError> {
        self.store.delete_secret(COPILOT_TOKEN_REF)?;
        Ok(())
    }

    fn put_forge_token(
        &self,
        owner: ForgeTokenOwner<'_>,
        value: &str,
        account: &ForgeAccount,
    ) -> Result<(), SecretError> {
        let stored = self.seal(&owner.aad(), value)?;
        let (project, header) = owner.row();
        self.store.put_forge_token(
            SecretRef {
                project: &project,
                server: "",
                header: &header,
            },
            LOCAL_BACKEND,
            &stored,
            owner,
            account,
        )?;
        Ok(())
    }

    fn put_project_forge_token(
        &self,
        owner: ForgeTokenOwner<'_>,
        value: &str,
        account: &ForgeAccount,
        guard: &ProjectGuard<'_>,
    ) -> Result<bool, SecretError> {
        let stored = self.seal(&owner.aad(), value)?;
        let (project, header) = owner.row();
        Ok(self.store.put_project_forge_token(
            SecretRef {
                project: &project,
                server: "",
                header: &header,
            },
            LOCAL_BACKEND,
            &stored,
            owner,
            account,
            guard,
        )?)
    }

    fn forge_token(&self, owner: ForgeTokenOwner<'_>) -> Result<Option<String>, SecretError> {
        let (project, header) = owner.row();
        let at = SecretRef {
            project: &project,
            server: "",
            header: &header,
        };
        let Some((backend, stored)) = self.store.get_secret(at)? else {
            return Ok(None);
        };
        Ok(Some(self.open(&backend, &stored, &owner.aad())?))
    }

    fn forge_token_with_account(
        &self,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount)>, SecretError> {
        Ok(self
            .forge_token_sealed(owner)?
            .map(|(token, account, _)| (token, account)))
    }

    fn forge_token_sealed(
        &self,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount, Vec<u8>)>, SecretError> {
        let (project, header) = owner.row();
        let at = SecretRef {
            project: &project,
            server: "",
            header: &header,
        };
        let Some((backend, stored, account)) = self.store.forge_token_snapshot(at, owner)? else {
            return Ok(None);
        };
        let token = self.open(&backend, &stored, &owner.aad())?;
        Ok(Some((token, account, stored)))
    }

    fn delete_forge_token(&self, owner: ForgeTokenOwner<'_>) -> Result<(), SecretError> {
        delete_forge_row(&self.store, owner)
    }
}

fn delete_forge_row(store: &Store, owner: ForgeTokenOwner<'_>) -> Result<(), SecretError> {
    let (project, header) = owner.row();
    store.delete_forge_token(
        SecretRef {
            project: &project,
            server: "",
            header: &header,
        },
        owner,
    )?;
    Ok(())
}

/// The backend used in place of [`LocalSecrets`] when no usable master key is
/// present. It cannot encrypt or decrypt, so `put` and `get` return
/// [`SecretError::Unavailable`], but `delete` still removes a stored row, so a
/// value can be cleared after the key is lost. The dashboard and the proxy
/// always hold a `SecretStore`, so they read [`available`](SecretStore::available)
/// rather than an `Option`.
pub struct UnavailableSecrets {
    store: Arc<Store>,
}

impl UnavailableSecrets {
    /// Builds the backend from the store that holds the `secrets` table.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        UnavailableSecrets { store }
    }
}

impl SecretStore for UnavailableSecrets {
    fn available(&self) -> bool {
        false
    }

    fn put(&self, _at: SecretRef<'_>, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }

    fn get(&self, _at: SecretRef<'_>) -> Result<Option<String>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn delete(&self, at: SecretRef<'_>) -> Result<(), SecretError> {
        self.store.delete_secret(at)?;
        Ok(())
    }

    fn claude_token(&self) -> Result<Option<String>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn set_claude_token(&self, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }

    fn clear_claude_token(&self) -> Result<(), SecretError> {
        self.store.delete_secret(CLAUDE_TOKEN_REF)?;
        Ok(())
    }

    fn codex_auth(&self) -> Result<Option<String>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn set_codex_auth(&self, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }

    fn clear_codex_auth(&self) -> Result<(), SecretError> {
        self.store.delete_secret(CODEX_AUTH_REF)?;
        Ok(())
    }

    fn copilot_token(&self) -> Result<Option<String>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn set_copilot_token(&self, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }

    fn clear_copilot_token(&self) -> Result<(), SecretError> {
        self.store.delete_secret(COPILOT_TOKEN_REF)?;
        Ok(())
    }

    fn put_forge_token(
        &self,
        _owner: ForgeTokenOwner<'_>,
        _value: &str,
        _account: &ForgeAccount,
    ) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }

    fn put_project_forge_token(
        &self,
        _owner: ForgeTokenOwner<'_>,
        _value: &str,
        _account: &ForgeAccount,
        _guard: &ProjectGuard<'_>,
    ) -> Result<bool, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn forge_token(&self, _owner: ForgeTokenOwner<'_>) -> Result<Option<String>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn forge_token_with_account(
        &self,
        _owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount)>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn forge_token_sealed(
        &self,
        _owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, ForgeAccount, Vec<u8>)>, SecretError> {
        Err(SecretError::Unavailable)
    }

    fn delete_forge_token(&self, owner: ForgeTokenOwner<'_>) -> Result<(), SecretError> {
        delete_forge_row(&self.store, owner)
    }
}

/// Resolves the Claude subscription token a worker run uses. With the secret
/// store available the token is the stored one, so a token saved on the
/// settings page takes effect without a restart. With the
/// store unavailable, because the master key is missing or malformed, it falls
/// back to the `CLAUDE_CODE_OAUTH_TOKEN` captured at startup, so a lost key
/// does not stop jobs while the old podman secret still passes the token in.
pub struct ClaudeToken {
    secrets: Arc<dyn SecretStore>,
    /// The startup `CLAUDE_CODE_OAUTH_TOKEN`, trimmed and non-empty, or `None`.
    /// It is the fallback when the store is unavailable and the source for the
    /// one-time import.
    env: Option<String>,
}

impl ClaudeToken {
    /// Builds the resolver from the secret store and the startup
    /// `CLAUDE_CODE_OAUTH_TOKEN`, which the caller trims and drops when empty.
    #[must_use]
    pub fn new(secrets: Arc<dyn SecretStore>, env: Option<String>) -> Self {
        ClaudeToken { secrets, env }
    }

    /// The token to use now, read afresh on each call, or `None` when there is
    /// none. With the store available it is the stored token, so the env var is
    /// ignored once a token is stored; a stored value that will not decrypt
    /// resolves to `None` and is logged without the value. With the store
    /// unavailable it is the env fallback.
    #[must_use]
    pub fn resolve(&self) -> Option<String> {
        if self.secrets.available() {
            match self.secrets.claude_token() {
                Ok(token) => token,
                Err(err) => {
                    tracing::warn!("reading the stored Claude token failed: {err}");
                    None
                }
            }
        } else {
            self.env.clone()
        }
    }

    /// The one-time import at startup. With the store available, no token
    /// stored, and a non-empty env value, it stores that value and logs that
    /// the token was imported, without the token itself. A token already stored
    /// is kept and the env var ignored, so a later start with a different env
    /// value does not overwrite it.
    pub fn import_env_token(&self) {
        if !self.secrets.available() {
            return;
        }
        let Some(env) = self.env.as_deref() else {
            return;
        };
        match self.secrets.claude_token() {
            Ok(Some(_)) => {}
            Ok(None) => match self.secrets.set_claude_token(env) {
                Ok(()) => tracing::info!(
                    "imported the Claude token from CLAUDE_CODE_OAUTH_TOKEN into the secret store"
                ),
                Err(err) => tracing::warn!("importing the Claude token failed: {err}"),
            },
            Err(err) => tracing::warn!("checking for a stored Claude token failed: {err}"),
        }
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

/// The associated data bound into the Claude token ciphertext. It is a fixed
/// domain tag, not the length-framed triple [`associated_data`] builds, so a
/// ciphertext moved between the token row and an MCP header row fails to
/// decrypt in either direction. The tag's first eight bytes read as a u64 far
/// larger than any header name's length, so it can never equal an MCP row's
/// associated data, which starts with the project name's length.
const CLAUDE_TOKEN_AAD: &[u8] = b"factory:claude-token:v1";

/// The associated data bound into the Codex login ciphertext. It is a fixed
/// domain tag distinct from [`CLAUDE_TOKEN_AAD`] and from the length-framed
/// triple [`associated_data`] builds, so a ciphertext moved between the Codex
/// row, the Claude token row and an MCP header row fails to decrypt in every
/// direction. Its first eight bytes read as a u64 far larger than any header
/// name's length, so it can never equal an MCP row's associated data.
const CODEX_AUTH_AAD: &[u8] = b"factory:codex-auth:v1";

const COPILOT_TOKEN_AAD: &[u8] = b"factory:copilot-token:v1";

/// The fixed tag every forge token's associated data starts with, followed by
/// the length-framed owner kind, key and forge. Like the other tags it reads
/// as a u64 far larger than any MCP header name's length.
const FORGE_TOKEN_AAD: &[u8] = b"factory:forge-token:v1";

/// Why a pasted or captured file was refused as a Codex subscription login.
/// None of these variants carry any part of the input, so a logged or
/// displayed error never echoes a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexAuthError {
    /// The input did not parse as a JSON object. An API key file, a Business
    /// access token or any non-JSON text lands here.
    NotJsonObject,
    /// `tokens.access_token` is missing or is not a string.
    AccessToken,
    /// `tokens.refresh_token` is missing or is not a string.
    RefreshToken,
}

impl fmt::Display for CodexAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodexAuthError::NotJsonObject => {
                write!(f, "the Codex login must be the JSON object from auth.json")
            }
            CodexAuthError::AccessToken => {
                write!(f, "the Codex login has no string at tokens.access_token")
            }
            CodexAuthError::RefreshToken => {
                write!(f, "the Codex login has no string at tokens.refresh_token")
            }
        }
    }
}

impl std::error::Error for CodexAuthError {}

/// Checks that `input` is a subscription `auth.json`: a JSON object with string
/// values at `tokens.access_token` and `tokens.refresh_token`. A `ChatGPT` Plus
/// or Pro `codex login` writes exactly that. An API key file, a Business access
/// token or any other shape is refused. The error names the missing field and
/// never carries any part of `input`, so the settings page can show it without
/// leaking a token.
///
/// # Errors
///
/// Returns [`CodexAuthError`] when `input` is not such an object.
pub fn validate_codex_auth(input: &str) -> Result<(), CodexAuthError> {
    let value: serde_json::Value =
        serde_json::from_str(input).map_err(|_| CodexAuthError::NotJsonObject)?;
    let object = value.as_object().ok_or(CodexAuthError::NotJsonObject)?;
    let tokens = object.get("tokens");
    let has_string = |key: &str| {
        tokens
            .and_then(|tokens| tokens.get(key))
            .and_then(serde_json::Value::as_str)
            .is_some()
    };
    if !has_string("access_token") {
        return Err(CodexAuthError::AccessToken);
    }
    if !has_string("refresh_token") {
        return Err(CodexAuthError::RefreshToken);
    }
    Ok(())
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
