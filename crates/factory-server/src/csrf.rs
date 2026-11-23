//! Form tokens for the forge token page. The login comes from the proxy, so
//! browser cookie rules do not protect the forms. A token is the login sealed
//! under a key made at startup, so another site cannot build one and a token
//! issued to one login fails for another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const NONCE_LEN: usize = 24;

pub struct CsrfKey(XChaCha20Poly1305);

impl CsrfKey {
    #[must_use]
    pub fn generate() -> Self {
        CsrfKey(XChaCha20Poly1305::new(&XChaCha20Poly1305::generate_key(
            &mut OsRng,
        )))
    }

    /// A token for forms rendered to `login`.
    ///
    /// # Panics
    ///
    /// Never in practice: sealing a short message does not fail.
    #[must_use]
    pub fn issue(&self, login: &str) -> String {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: b"csrf",
                    aad: login.as_bytes(),
                },
            )
            .expect("sealing a short message cannot fail");
        let mut bytes = nonce.to_vec();
        bytes.extend_from_slice(&sealed);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Whether `token` was issued by this key to `login`.
    #[must_use]
    pub fn accepts(&self, login: &str, token: &str) -> bool {
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(token.trim()) else {
            return false;
        };
        if bytes.len() <= NONCE_LEN {
            return false;
        }
        let (nonce, sealed) = bytes.split_at(NONCE_LEN);
        self.0
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: login.as_bytes(),
                },
            )
            .is_ok_and(|plain| plain == b"csrf")
    }
}
