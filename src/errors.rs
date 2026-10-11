//! The typed error surface of the library.
//!
//! Every fallible library function returns `Result<_, Error>`; the CLI is the
//! only place an `Error` becomes a process exit code, via [`Error::exit_code`].
//! The code table is the tool's stable contract:
//!
//! | code | variant |
//! |------|---------|
//! | 1 | [`Error::Usage`] |
//! | 2 | [`Error::Artifact`] |
//! | 3 | [`Error::Drift`] |
//! | 4 | [`Error::Verify`], [`Error::Firewall`] |
//! | 5 | [`Error::Llm`] |
//! | 6 | [`Error::Fs`] |
//! | 7 | [`Error::Invariant`] |
//!
//! Ticket text of the form "exit N" inside library modules means "return the
//! `Error` variant that maps to N".

use std::path::PathBuf;

use thiserror::Error;

/// Why an LLM call failed. Control flow branches on the reason, never on
/// message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LlmErrorReason {
    /// `finish_reason == "length"`: the response was truncated.
    #[error("truncated response")]
    Truncated,
    /// The model refused to answer.
    #[error("refusal")]
    Refusal,
    /// HTTP 401/403/404: bad credentials, unknown model or endpoint.
    #[error("auth failure")]
    Auth,
    /// Retries exhausted.
    #[error("retries exhausted")]
    Exhausted,
    /// The per-request timeout fired.
    #[error("timeout")]
    Timeout,
    /// Non-retryable HTTP status, or a 200 response whose envelope is
    /// unusable.
    #[error("http error")]
    Http,
    /// Output failed validation even after the repair retry.
    #[error("invalid model output")]
    Invalid,
}

/// The one error enum of the library, carrying each variant's exit code.
#[derive(Debug, Error)]
pub enum Error {
    /// CLI usage error, or an invalid library request field (exit 1).
    #[error("usage error: {0}")]
    Usage(String),
    /// Artifact missing, invalid, or in disagreement with the codebase
    /// (exit 2).
    #[error("artifact error: {0}")]
    Artifact(String),
    /// Codebase drift: a file's current content no longer matches the
    /// artifact (exit 3).
    #[error("codebase drift: {0}")]
    Drift(String),
    /// Semantic verification failure (exit 4).
    #[error("semantic verification failed: {0}")]
    Verify(String),
    /// Apply-time firewall rejection (exit 4).
    #[error("firewall rejection: {0}")]
    Firewall(String),
    /// LLM/endpoint error (exit 5).
    #[error("llm error ({reason}): {message}")]
    Llm {
        /// Classification of the failure.
        reason: LlmErrorReason,
        /// Human-readable detail.
        message: String,
    },
    /// Filesystem I/O error (exit 6).
    #[error("filesystem error at {path}: {source}")]
    Fs {
        /// The path involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Internal invariant violation — a bug canary (exit 7).
    #[error("internal invariant violated: {0}")]
    Invariant(String),
}

impl Error {
    /// The stable exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) => 1,
            Error::Artifact(_) => 2,
            Error::Drift(_) => 3,
            Error::Verify(_) | Error::Firewall(_) => 4,
            Error::Llm { .. } => 5,
            Error::Fs { .. } => 6,
            Error::Invariant(_) => 7,
        }
    }
}

/// Exit code when several failures occurred in one run.
///
/// The documented precedence is `2 > 3 > 7 > 4 > 5 > 6`: validation and drift
/// abort before anything is attempted, and per-file failures surface over
/// transport and I/O noise. `130` (interruption) sits outside the precedence
/// and is never an input here. An empty slice means "no failure" and yields
/// 0. Codes outside the documented set — none of the current callers produce
/// them — sort after every listed code, first occurrence winning, so the
/// function stays total and deterministic.
pub fn pick_exit_code(codes: &[u8]) -> u8 {
    const PRECEDENCE: [u8; 6] = [2, 3, 7, 4, 5, 6];
    for code in PRECEDENCE {
        if codes.contains(&code) {
            return code;
        }
    }
    codes.first().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_documented_exit_code() {
        assert_eq!(Error::Usage(String::new()).exit_code(), 1);
        assert_eq!(Error::Artifact(String::new()).exit_code(), 2);
        assert_eq!(Error::Drift(String::new()).exit_code(), 3);
        assert_eq!(Error::Verify(String::new()).exit_code(), 4);
        assert_eq!(Error::Firewall(String::new()).exit_code(), 4);
        assert_eq!(
            Error::Llm {
                reason: LlmErrorReason::Auth,
                message: String::new(),
            }
            .exit_code(),
            5,
        );
        assert_eq!(
            Error::Fs {
                path: PathBuf::new(),
                source: std::io::Error::other("disk"),
            }
            .exit_code(),
            6,
        );
        assert_eq!(Error::Invariant(String::new()).exit_code(), 7);
    }

    #[test]
    fn pick_exit_code_follows_the_documented_precedence() {
        // empty (and success-only) input
        assert_eq!(pick_exit_code(&[]), 0);
        assert_eq!(pick_exit_code(&[0]), 0);
        // 2 beats everything
        assert_eq!(pick_exit_code(&[6, 3, 2, 7, 4, 5]), 2);
        // 3 beats everything except 2
        assert_eq!(pick_exit_code(&[3, 7, 4, 5, 6]), 3);
        assert_eq!(pick_exit_code(&[5, 3]), 3);
        // 7 beats 4/5/6
        assert_eq!(pick_exit_code(&[6, 7]), 7);
        assert_eq!(pick_exit_code(&[4, 7]), 7);
        // 4 beats 5/6
        assert_eq!(pick_exit_code(&[5, 4]), 4);
        assert_eq!(pick_exit_code(&[4, 5, 6]), 4);
        // 5 beats 6
        assert_eq!(pick_exit_code(&[5, 6]), 5);
        assert_eq!(pick_exit_code(&[6, 5]), 5);
        assert_eq!(pick_exit_code(&[6, 5, 4, 7, 3, 2]), 2);
    }

    #[test]
    fn pick_exit_code_is_deterministic_for_unlisted_codes() {
        assert_eq!(pick_exit_code(&[1]), 1);
        assert_eq!(pick_exit_code(&[1, 9]), 1);
        assert_eq!(pick_exit_code(&[9, 1]), 9);
        // a listed code always wins over an unlisted one
        assert_eq!(pick_exit_code(&[1, 6]), 6);
    }
}
