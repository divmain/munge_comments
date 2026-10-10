//! The LLM boundary of the tool.
//!
//! Everything that talks to an endpoint sits behind the [`LlmClient`] trait,
//! so the engine stays fully testable with a fake. Phase 5 adds the
//! transport, typed calls, wire schemas, prompts and chunking.

pub mod client;

pub use client::LlmClient;
