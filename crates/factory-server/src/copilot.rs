//! The factory's Copilot login: checking a token before it is stored.
//!
//! The Copilot CLI authenticates with a GitHub token. It accepts an OAuth
//! token (`gho_`, what `copilot login` stores), a user-to-server token
//! (`ghu_`) and a fine-grained personal access token (`github_pat_`) with the
//! "Copilot Requests" permission. It does not accept a classic personal access
//! token (`ghp_`).

use std::fmt;

/// Why a token was refused. None of these variants carry any part of the
/// input, so a logged or displayed error never echoes a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotTokenError {
    /// The input was empty after trimming.
    Empty,
    /// A classic personal access token, which the Copilot CLI refuses.
    Classic,
    /// The input did not start with a supported prefix, or had nothing after it.
    UnknownFormat,
}

impl fmt::Display for CopilotTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopilotTokenError::Empty => write!(f, "the Copilot token is empty"),
            CopilotTokenError::Classic => write!(
                f,
                "classic personal access tokens are not supported; use a fine-grained token with the Copilot Requests permission"
            ),
            CopilotTokenError::UnknownFormat => write!(
                f,
                "the Copilot token must start with gho_, ghu_ or github_pat_ and have a value after the prefix"
            ),
        }
    }
}

impl std::error::Error for CopilotTokenError {}

/// Trims `input` and checks that it is a token the Copilot CLI accepts: a
/// `gho_`, `ghu_` or `github_pat_` token with something after the prefix.
/// Returns the trimmed token. The error never carries any part of `input`.
///
/// # Errors
///
/// Returns [`CopilotTokenError`] when `input` is empty, is a classic `ghp_`
/// token, or has another format.
pub fn validate_copilot_token(input: &str) -> Result<&str, CopilotTokenError> {
    let token = input.trim();
    if token.is_empty() {
        return Err(CopilotTokenError::Empty);
    }
    if token.starts_with("ghp_") {
        return Err(CopilotTokenError::Classic);
    }
    let has_body = ["gho_", "ghu_", "github_pat_"].iter().any(|prefix| {
        token
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty())
    });
    if has_body {
        Ok(token)
    } else {
        Err(CopilotTokenError::UnknownFormat)
    }
}
