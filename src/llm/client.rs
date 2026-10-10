//! The LLM client contract.
//!
//! Phase 1 ships the object-safe marker trait only —
//! [`Io`](crate::cli::Io) carries an `Option<Arc<dyn LlmClient>>` from day
//! one. Phase 5 adds `complete_json` (the single method the engine calls),
//! the OpenAI-compatible transport, and the `FakeLlm` test double.

/// A chat-completions backend. Object-safe, so `Arc<dyn LlmClient>` admits
/// the built-in transport, the test fake, and consumer-provided clients
/// alike.
pub trait LlmClient: Send + Sync {}
